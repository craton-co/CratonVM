#[allow(unused_imports)]
use super::*;

/// Body of [`jit_service_callee_deopt`], run under its panic guard.
///
/// `num_args` is a plain count. Between 2026-09-22 and 2026-09-22 its bit 32
/// carried a flag — set by `emit_inline_callee_deopt_check`'s
/// `is_self_recursive` arm, read here as `trust_throw_pc` — whose only job was
/// to opt this call OUT of a stamped-pc downgrade in `run_jit_callee_handler`.
/// That downgrade turned out to be the defect it was meant to work around (it
/// matched a TYPED handler through the pc-unknown search regardless of the
/// handler's protected range), so it was deleted and the bit with it. An ABI
/// bit nobody decodes reads as a signal that is being honoured; there is no
/// second reader to inherit it.
///
/// gcd d9/d (2026-09-28): one WHOLE-word value is not a count:
/// [`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`] (negative, so no count can
/// be it), passed with a null `args_ptr` by a single-pass direct call that
/// kept no copy of its arguments because they belong to the callee
/// (`x64/op_invoke.rs`, `sp_direct_call_args_owned_by_callee`). It is decoded
/// here and handed to [`handle_compiled_callee_deopt_sentinel`] as
/// `args_owned_by_callee`.
pub unsafe fn jit_service_callee_deopt_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // Rust<->JIT boundary — same bookkeeping the dispatch helpers do.
    crate::jit::conservative_roots::note_jit_boundary();
    // No flag bits any more: every emitter passes the plain count. See the
    // doc above for the bit that used to live at 32 and why it is gone.
    if vm_ptr == 0 || info_ptr == 0 {
        // r10 `help`: raise the out-of-band deopt flag with the sentinel. A
        // bare `i64::MIN` with every signal CLEAR is not a sentinel at all to
        // the interpreter — `jit_bridge`'s post-JIT path gates its deopt arm on
        // `deopt_signaled` and otherwise pushes the value as a REAL return
        // ("A bare `result == i64::MIN` with the flag CLEAR is a method
        // legitimately returning `Long.MIN_VALUE` ... and must be pushed as a
        // real value below"). For an `L`-returning frame that is a wild
        // pointer in an oop slot.
        set_jit_deopt_pending();
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    // JIT round 19 wave 4, lane throws4 (THROWS3-3): a direct self-call's
    // lean sentinel path ORs `SERVICE_ARGS_LEAN_PROPAGATE_BIT` into a plain
    // (non-negative) count. Masked off before anything reads the count.
    // Wave 5, lane throws5 (THROWS4-1): every statically bound direct-call
    // route may set it, and a single-pass site whose callee owns the
    // arguments sets it with `SERVICE_ARGS_LEAN_ARGS_OWNED_BIT`, decoded back
    // to `SERVICE_ARGS_OWNED_BY_CALLEE`.
    let (num_args, lean) = cratonvm_jit::decode_service_args_word(num_args);
    if lean && service_has_nothing_to_do_but_propagate(vm, info) {
        return i64::MIN;
    }
    // gcd d9/d: the site kept no argument copy; `n` is then 0 and the slice
    // empty, and nothing below may read arguments.
    let args_owned_by_callee = num_args == i64::from(cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE);
    let n = if num_args > 0 { num_args as usize } else { 0 };
    let args_slice: &[i64] = if n == 0 || args_ptr == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, n)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        // As the arm above: the sentinel needs a signal beside it or the
        // interpreter reads it as this method's return value.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    // The receiver's class id, for the callee-exception-table probe.
    //
    // Only kinds 0/2 have one. For a statically bound site (`invokestatic` /
    // `invokespecial`) arg 0 is an ordinary argument, and reading it as a
    // receiver resolves the callee from whatever class that argument happens to
    // point at — a different method with the same name and descriptor, or none.
    // `route_implicit_exc_through_callee` and `resolve_callee_cached` both
    // already branch on `invoke_kind` for exactly this reason; this arm did
    // not, and the direct-call door (`x64/bytecode_walk.rs`, the
    // `emit_inline_callee_deopt_check` after a baked `invokestatic` CALL) is a
    // live caller of it. `ClassId::new(0)` is the "no receiver" value the
    // callee-side probe below reads as "resolve by name".
    // An array receiver dispatches on `java/lang/Object`, which is what the
    // by-name resolution from the site's class answers; its header's
    // COMPONENT id would resolve the component's override instead
    // (`jit_dispatch_class`).
    let receiver_class_id = if matches!(info.invoke_kind, 0 | 2) {
        args_slice
            .first()
            .and_then(|raw| vm.mem.heap.is_object_address(*raw as usize))
            .and_then(|obj| jit_dispatch_class_validated(vm, obj).class_id())
            .unwrap_or_else(|| ClassId::new(0))
    } else {
        ClassId::new(0)
    };
    // No pins: this door is entered AFTER the callee returned (see
    // r11w4-rt-callee-rerun-reads-pre-call-argument-words).
    match handle_compiled_callee_deopt_sentinel(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        None,
        args_owned_by_callee,
    ) {
        Some(v) => v,
        None => {
            // The sentinel is about to keep propagating. If a stash is STILL
            // present at this point it is leaving the one call site that could
            // attribute it — every consumer above here sees a frame belonging
            // to a method it did not invoke. Name it here rather than at the
            // sink, where the caller chain is already gone.
            if cratonvm_jit::deopt::has_last_deopt() && dbg_deopt_on() {
                if let Some((key, bci)) = cratonvm_jit::deopt::peek_last_deopt_identity() {
                    eprintln!(
                        "[cratonvm-deopt] ORPHANED at the serviced call site {}{}: \
                         stash={key} bci={bci}",
                        info.method_name, info.descriptor,
                    );
                }
            }
            i64::MIN
        }
    }
}

/// The VM half of a NOT ENTRANT compiled body (interpreter round i1 wave 22,
/// lane L6; `jit/src/not_entrant.rs`).
///
/// A redefinition patches the entry of every compiled body it makes stale
/// with a jump to that body's stub, and the stub calls this with the body's
/// [`cratonvm_jit::NotEntrantRecord`], the argument registers it spilled and
/// the caller's stack-passed arguments. Whoever made the call -- a baked
/// direct `CALL` of either tier, an instance-site bind, a stale inline-cache
/// word, a Rust door -- is answered here: the call is re-dispatched by name
/// through the interpreter's invoke path (`bail_to_interpreter`, non-virtually,
/// since the caller already selected exactly this method), which runs the
/// class's current bytecode or its newest compiled body. The result goes back
/// in the compiled ABI; a throw goes back as the `i64::MIN` sentinel with the
/// pending exception stashed, exactly as a compiled callee that threw, which
/// every caller already handles. HotSpot's `handle_wrong_method` stub answers
/// the same entry; it also re-binds the caller's call site. Since interpreter
/// round i1 wave 23 (lane L6) this re-binds the STUB instead: it hands the
/// body now published under the same key to
/// [`cratonvm_jit::NotEntrantRecord::forward_to`], and every later call jumps
/// from the stub straight to that body without coming here.
///
/// # Safety
///
/// Called only by a not-entrant stub: `record` is the live record of the body
/// whose entry was patched (it lives as long as that body's code, and the
/// stub's frame is on this stack), `regs` points at the spilled argument
/// registers and `stack_args` at the caller's stack-passed arguments of a call
/// made into that body.
#[no_mangle]
pub unsafe extern "C" fn jit_not_entrant_entry(record: i64, regs: i64, stack_args: i64) -> i64 {
    // The VM the body belongs to, for the panic report.
    let vm_ptr = if record == 0 {
        0
    } else {
        // SAFETY: per this function's contract `record` is a live
        // `NotEntrantRecord`.
        // Cast: `vm_ptr` is an address, carried as the helpers' `i64` VM word.
        unsafe { (*(record as *const cratonvm_jit::NotEntrantRecord)).vm_ptr as i64 }
    };
    let f = || {
        // SAFETY: forwarded unchanged under this function's own contract.
        unsafe { jit_not_entrant_entry_body(record, regs, stack_args) }
    };
    contain(
        "jit_not_entrant_entry",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_not_entrant_entry`], run under its panic guard.
///
/// # Safety
///
/// As [`jit_not_entrant_entry`].
pub unsafe fn jit_not_entrant_entry_body(record: i64, regs: i64, stack_args: i64) -> i64 {
    // Rust<->JIT boundary — same bookkeeping the dispatch helpers do.
    crate::jit::conservative_roots::note_jit_boundary();
    if record == 0 || regs == 0 {
        // No record: nothing names the method. The sentinel needs a signal
        // beside it or the caller reads it as a returned `Long.MIN_VALUE`.
        set_jit_deopt_pending();
        return i64::MIN;
    }
    // SAFETY: per the contract, a live record.
    let record = unsafe { &*(record as *const cratonvm_jit::NotEntrantRecord) };
    if record.vm_ptr == 0 {
        set_jit_deopt_pending();
        return i64::MIN;
    }
    // SAFETY: the record's VM is the `SharedVm` that armed the patch; a VM
    // outlives every compiled body of its cache.
    let vm = unsafe { &*(record.vm_ptr as *const SharedVm) };
    // SAFETY: `regs` / `stack_args` are what the stub passed for a call into
    // this record's body (the contract), so every argument the body's own
    // prologue would read is there.
    let raw = unsafe { record.java_args(regs as *const i64, stack_args as *const i64) };
    // SAFETY: a helper on the thread that entered compiled code.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        set_jit_deopt_pending();
        return i64::MIN;
    };
    // Entry hygiene, as `call_compiled_entry_under_owner` applies it before a
    // compiled callee runs: a caller issuing a call is not in an exceptional
    // state, so a deopt flag pending now describes no part of this call, and
    // left set it would turn a genuine `Long.MIN_VALUE` result into a
    // sentinel at the caller.
    let _ = take_jit_deopt_pending();
    // The stub built no frame: publish the intercepted call's return address
    // as the innermost compiled frame's pc for the re-dispatch, so a trace
    // captured by the new code places the caller by its return site
    // (`r13w1-callcost-not-entrant-innermost-native-pc-patch-FIXED-20260928.md`).
    // SAFETY: `stack_args` is the pointer the stub passed (the contract).
    let ret_pc =
        unsafe { cratonvm_jit::NotEntrantRecord::caller_return_address(stack_args as *const i64) };
    let prev_ret_pc = crate::jit::conservative_roots::swap_top_not_entrant_ret_pc(ret_pc);
    struct RestoreRetPc(usize);
    impl Drop for RestoreRetPc {
        fn drop(&mut self) {
            crate::jit::conservative_roots::swap_top_not_entrant_ret_pc(self.0);
        }
    }
    let _restore_ret_pc = RestoreRetPc(prev_ret_pc);
    // Interpreter round i1 wave 23, lane L6: point the stub at the body now
    // published under the same key, so the caller's next call jumps straight
    // there instead of coming back here (HotSpot re-binds the caller's call
    // site for the same reason; `NotEntrantRecord::forward_to` says why the
    // stub is re-bound instead). This call is still answered through the VM
    // below. Not while an agent may need any method interpreted (a JVMTI
    // interpreter-only event, an armed JDWP gate): a forward bypasses the
    // doors that ask.
    // Wave 24 (lane L6): nor to a method a statically bound call must reach
    // through the dispatch helper now (a recursion cycle closed by an inline
    // cache, `JitVerdictRegistry::direct_call_requires_dispatch_of`) -- the
    // verdict a caller compiled today would ask before it bakes the call, and
    // one that may have been reached after this record's callers were compiled.
    let forwarded = {
        let info = &record.info;
        let class_id = ClassId::new(info.declaring_class_id);
        info.declaring_class_id != 0
            && !crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm)
            && !vm
                .jit
                .tiered_manager
                .verdicts()
                .direct_call_requires_dispatch_of(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    || Some(class_id),
                )
            && vm
                .jit
                .jit_cache
                .get(info.class_name, info.method_name, info.descriptor, class_id)
                // Through the cache: a forward that closes a reference cycle
                // (the strings round's `copyOfRange` -> `StringBuilder` ->
                // `String.<init>` -> stale `copyOfRange`) is installed and
                // registered there, and released when its target is withdrawn.
                .is_some_and(|current| vm.jit.jit_cache.forward_not_entrant(record, &current))
    };
    // JIT round 20 wave 19, lane yStash (STASH20-2,
    // `CRATONVM_JIT_STALE_CALL_REBIND`): with the forward installed, re-point
    // the caller's own `CALL rel32` at the body it forwards to, as HotSpot's
    // wrong-method stub re-resolves the caller's call site.
    let rebound = forwarded && stale_call_rebind_on_stub_pass(vm, record, ret_pc);
    // JIT round 20 wave 11, lane pLate (W16L1-3,
    // `CRATONVM_JIT_STALE_CALLER_RECOMPILE`): the caller that baked this
    // stale entry gets ONE recompile, so its next body binds the current
    // callee instead of paying the stub on every call. Bounded per caller
    // artifact; see the function. Not for a caller whose call site was just
    // rebound (STASH20-2): it no longer pays the stub.
    if !rebound {
        let info = &record.info;
        crate::runtime::interpreter::jit_bridge::stale_caller_recompile_on_stub_pass(
            vm,
            ret_pc,
            (
                info.class_name,
                info.method_name,
                info.descriptor,
                info.declaring_class_id,
            ),
        );
    }
    // Round 16 wave 16 (lane chains2, TF15-2): counted per record, printed at
    // the first call and every power of two (`n=` is the running count).
    let redispatch_line_due = if dbg_deopt_on() {
        record.note_redispatch()
    } else {
        None
    };
    if let Some(n) = redispatch_line_due {
        // Why the stub is not forwarding, re-asked term by term (only when
        // the line prints): a record that names no reason here keeps every
        // baked call on the by-name path below, the r16 strings bimodality.
        let refusal = if forwarded {
            String::new()
        } else {
            let info = &record.info;
            let class_id = ClassId::new(info.declaring_class_id);
            let reason = if info.declaring_class_id == 0 {
                "no declaring class id"
            } else if crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm) {
                "compiled frames may be asked to leave"
            } else if vm
                .jit
                .tiered_manager
                .verdicts()
                .direct_call_requires_dispatch_of(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    || Some(class_id),
                )
            {
                "a direct call must dispatch this method"
            } else {
                match vm.jit.jit_cache.get(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    class_id,
                ) {
                    None => "no current body published",
                    Some(current) => record.explain_forward_refusal(&current),
                }
            };
            format!("; no forward: {reason}")
        };
        eprintln!(
            "[cratonvm-deopt] not-entrant entry: re-dispatching {}.{}{} ({} args) \
             n={n} from ret={:#x}{}{refusal}",
            record.info.class_name,
            record.info.method_name,
            record.info.descriptor,
            raw.len(),
            // SAFETY: `stack_args` is the pointer the stub passed (the contract).
            unsafe {
                cratonvm_jit::NotEntrantRecord::caller_return_address(stack_args as *const i64)
            },
            if forwarded {
                "; later calls forward to the current body"
            } else {
                ""
            },
        );
    }
    // SAFETY: `vm` live, `record.info` a live `JitInvokeInfo` owned by the
    // record, `raw` the call's own argument words.
    let args = unsafe { decode_dispatch_values(vm, &record.info, &raw) };
    let info = &record.info;
    if info.invoke_kind == 3 && info.declaring_class_id != 0 {
        // A static method: on the class the body was published under, by id,
        // not by name -- `bail_to_interpreter`'s static arm resolves the name
        // through the global map, which can answer another loader's copy (or
        // nothing, for a hidden class). JVMS 5.4.3.3 lookup from that class,
        // and 5.5 initialization of the declaring class, as
        // `jit_invoke_dispatch`'s loader-faithful static arm does.
        let res = crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            ClassId::new(info.declaring_class_id),
            info.class_name,
            info.method_name,
            info.descriptor,
            &args,
        );
        let rc = match res {
            // Cast: the compiled ABI returns an `int` sign-extended in RAX.
            Ok(Some(Value::Int(v))) => v as i64,
            Ok(Some(Value::Long(v))) => v,
            // Cast: floating-point results travel as their bits.
            Ok(Some(Value::Float(f))) => f.to_bits() as i64,
            // Cast: as above.
            Ok(Some(Value::Double(d))) => d.to_bits() as i64,
            // Cast: a reference travels as its address.
            Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
            Ok(Some(Value::Object(None))) | Ok(None) => 0,
            Ok(_) => 0,
            Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
        };
        // gce e1/f: a throw here has already left the method.
        return note_redispatch_escape(thread, info, rc);
    }
    // An instance method: `invoke_kind` 1 on the substituted owner (the
    // record names its class id), i.e. `invoke_special_shared_on_class`.
    // SAFETY: as above; `thread` is this thread's exclusive borrow.
    let rc = unsafe { bail_to_interpreter(vm, thread, info, &args) };
    // gce e1/f: as the static arm.
    note_redispatch_escape(thread, info, rc)
}



/// STASH20-2: a call returning to `ret_pc` has just come through `record`'s
/// not-entrant stub, and [`jit_not_entrant_entry`] installed the stub's
/// forward to the method's current body. Re-point the caller's direct `CALL
/// rel32` at that body (`cratonvm_jit::JitCache::rebind_stale_call_site`, which
/// checks the site, the lifetime and the word, and writes it atomically
/// under the cache's mutation lock), so the caller stops paying the patched
/// `JMP` plus the forwarding prefix on every call: the `not-entrant-stub
/// <method>` share in `perf` (1.2% on nio21 w15 `bufascii`). HotSpot's
/// `handle_wrong_method` re-resolves the caller's call site the same way.
///
/// The first rebound sites of a record also clear its forward word, so the
/// NEXT stale caller comes through here once and is rebound too; a call that
/// cannot be rebound (a Rust door, an inline cache, a straddling site) leaves
/// the forward installed for everyone, as before. Returns whether the site was
/// rebound. Under `CRATONVM_DBG_JITC`: `stale-call REBOUND` / `stale-call
/// rebind DECLINED` lines.
pub fn stale_call_rebind_on_stub_pass(
    vm: &SharedVm,
    record: &cratonvm_jit::NotEntrantRecord,
    ret_pc: usize,
) -> bool {
    if ret_pc == 0 {
        return false;
    }
    let info = &record.info;
    let Some(target) = vm
        .jit
        .jit_cache
        .get(
            info.class_name,
            info.method_name,
            info.descriptor,
            ClassId::new(info.declaring_class_id),
        )
        .map(cratonvm_jit::RetainedCode::new)
    else {
        return false;
    };
    // The caller's frame is live (the stub returns into it), so its body is
    // pinned by the code-range registry; released through the retirement
    // queue. `None`: a Rust door or a stub, nothing to rebind.
    let Some(caller) =
        cratonvm_jit::pin_jit_code_range_owner(ret_pc).map(cratonvm_jit::RetainedCode::new)
    else {
        return false;
    };
    let answer = vm
        .jit
        .jit_cache
        .rebind_stale_call_site(record, target.arc(), &caller, ret_pc);
    if crate::runtime::env_cache::dbg_jitc() {
        match answer {
            Ok(cleared) => eprintln!(
                "[cratonvm-jitc] stale-call REBOUND {}.{}{} in {} (ret={ret_pc:#x}) -> entry={:#x}{}",
                info.class_name,
                info.method_name,
                info.descriptor,
                caller.method_label,
                // Cast: an address, for the trace line.
                target.entry_ptr() as usize,
                if cleared {
                    "; forward word cleared for the next stale caller"
                } else {
                    ""
                },
            ),
            Err(why) => eprintln!(
                "[cratonvm-jitc] stale-call rebind DECLINED {}.{}{} in {} (ret={ret_pc:#x}, caller entry={:#x} artifact={}): {why}",
                info.class_name,
                info.method_name,
                info.descriptor,
                caller.method_label,
                // Cast: an address, for the trace line.
                caller.entry_ptr() as usize,
                caller.artifact_id,
            ),
        }
    }
    answer.is_ok()
}

/// Handle the `i64::MIN` deopt/exception sentinel returned by a direct call
/// into a compiled callee.
///
/// Shared by the direct-entry arms of `jit_invoke_virtual_mic` — the
/// monomorphic inline cache and the megamorphic PIC, through
/// [`handle_rust_armed_callee_sentinel`] — and by the inline cascade's
/// `jit_service_callee_deopt`. They MUST agree: a
/// sentinel returned unhandled is indistinguishable, to the compiled caller,
/// from the caller's own deopt, so the callee's stashed frame travels up to an
/// unrelated sink that de-speculates the wrong method and cannot resume.
///
/// Returns `Some(v)` when the trap was fully serviced here (`v` is the call's
/// result), `None` when the sentinel should propagate unchanged.
///
/// `pins`, when the caller rooted `args_slice` across the call, is what the
/// handler runs read the arguments back through ([`args_now`]); `None` reads
/// `args_slice` as given (see r11w4-rt-callee-rerun-reads-pre-call-argument-words
/// for the arms that cannot pin yet).
///
/// `args_owned_by_callee` (gcd d9/d, 2026-09-28): the call site kept no copy
/// of its arguments (`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`), so
/// `args_slice` is empty and no arm may read arguments. The precise resume
/// and the exception pass-through are unchanged. The three arms that read
/// arguments back after the call -- the declined-stash re-run, the frameless
/// re-run and the callee's own handler -- are not taken: the trap leaves as a
/// frameless deopt of the CALLER (the declined stash is dropped first, its
/// locks released and its method charged, as the re-run would have), and the
/// caller's caller re-runs the caller, which is how a frameless trap of the
/// caller itself is answered. That re-run commits nothing twice: since gcd
/// d10/f a site claims ownership only where the caller's bytecode before the
/// call (closed over loops) makes no store, invoke or monitor operation
/// (`x64/op_invoke.rs`, `owned_args_handoff_replays_exactly`; see
/// `gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md`), so
/// the only replay left is the callee's own, as at a copying site. The site
/// only claims ownership for a callee
/// whose method has no exception table and whose body has no frameless trap
/// stub (`cratonvm_jit::callee_body_owns_its_arguments`), so those arms are
/// the rare declines; a handler found anyway (a redefinition added one) is
/// answered the same way, with the pending throwable dropped, since the
/// re-run raises it again. Each such hand-off is named under
/// `CRATONVM_DBG_DEOPT` ([`note_callee_owned_args_trap_propagated`]).
///
/// SAFETY: same contract as the callers — `vm` live, `info` a live
/// `JitInvokeInfo`, `thread` the current thread's exclusive borrow.
///
/// HANDLER11-3 (JIT round 19 wave 12, lane handler12): a call this service
/// COMPLETES with a value while the stash it saw on entry is still the
/// innermost entry left an orphan; [`note_completed_call_left_stash`] names it
/// (`callee-deopt-service`) and, under its opt-in switch, drops it.
pub unsafe fn handle_compiled_callee_deopt_sentinel(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
    args_owned_by_callee: bool,
) -> Option<i64> {
    let entry_top = cratonvm_jit::deopt::deopt_stash_top_seq();
    let answer = handle_compiled_callee_deopt_sentinel_inner(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        pins,
        args_owned_by_callee,
    );
    if answer.is_some_and(|v| v != i64::MIN) {
        note_completed_call_left_stash(entry_top, "callee-deopt-service");
    }
    answer
}

/// `CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH` (HANDLER11-3, JIT round 19
/// wave 12, lane handler12; **default OFF**, `1` enables): a consumer that
/// completes a call over the deopt stash entry it saw on entry discards that
/// entry instead of only naming it ([`note_completed_call_left_stash`]). Read
/// only when such an orphan is born (cold).
pub fn completed_call_drops_left_stash_enabled() -> bool {
    false
}

/// HANDLER11-3 (JIT round 19 wave 12, lane handler12;
/// `r19-outside-manager-c2-upgrade-crashes-under-load-FIXED-20261012.md`): a consumer
/// of a callee's sentinel has completed the call -- its caller gets a value,
/// or (at a door) the call's throwable -- and `entry_top` is the innermost
/// stash entry it saw when it began (`cratonvm_jit::deopt::deopt_stash_top_seq`).
/// If that entry is still innermost, no sentinel travels on to a sink that
/// could take it: it is an orphan, born here. Stamped with `label` (every
/// later discard names it under `CRATONVM_DBG_DEOPT`, so one run of the
/// loaded stress names the consumer that leaves R16LocaleWide's orphan), and
/// discarded at once under `CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH`.
/// One thread-local read on a completing sentinel path; nothing when the
/// entry stash was empty.
pub fn note_completed_call_left_stash(entry_top: Option<u64>, label: &'static str) {
    if entry_top.is_none() {
        return;
    }
    cratonvm_jit::deopt::note_stash_left_by(
        entry_top,
        label,
        completed_call_drops_left_stash_enabled,
    );
}

/// [`handle_compiled_callee_deopt_sentinel`] without the HANDLER11-3 orphan
/// note.
///
/// SAFETY: as [`handle_compiled_callee_deopt_sentinel`].
pub unsafe fn handle_compiled_callee_deopt_sentinel_inner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
    args_owned_by_callee: bool,
) -> Option<i64> {
    // Precise resume of a frame-stashing deopt in the dispatched callee (see
    // `try_resume_trapped_callee`). Must run before consuming the exception
    // signals below: a pure deopt sets no exception flags.
    if let Some(v) = try_resume_trapped_callee(
        vm,
        thread,
        info,
        receiver_class_id,
        // Pinned callers hand a Rust copy; only the compiled caller's own
        // block (no pins: this door runs after the callee returned) is named
        // in its oop map.
        (!args_owned_by_callee && pins.is_none()).then_some(args_slice),
    ) {
        return Some(v);
    }

    // A machine-call cache hit returns the callee's sentinel in the caller's
    // ABI.  Drain the *whole* signal record here, while the receiver class and
    // outgoing arguments still identify that callee, and run its handler at
    // the recorded throw bci.  Re-entering the callee from bytecode 0 used to
    // duplicate all side effects before a caught bounds/null/divide exception.
    // gce e1/f: taken first -- `take_all_jit_signals` clears it. Kept raw
    // (interpreter round i5 wave 104, lane L2): the lambda arm below asks it
    // about the IMPL as well as about `info`'s SAM method.
    let escape_record = take_redispatch_escape_record();
    let escaped_redispatch =
        redispatch_escape_record_names(escape_record, info.method_name, info.descriptor);
    let mut signals = take_all_jit_signals(thread);
    // gcd d9/d: set once an owned-arguments trap has been handed to the
    // caller's caller, so it is counted once.
    let mut owned_args_propagated = false;
    // gce e1/f: the replay verdict of the caller's re-run a hand-off asks for
    // (`FramelessTrapSite::Handoff`), `None` when this is no hand-off or the
    // callee's own replay could not be judged.
    let mut handoff_exact: Option<bool> = None;
    // A pure deopt (no throwable, no implicit flag) whose stash
    // `try_resume_trapped_callee` declined above: re-run this site's callee
    // from entry instead of letting its frame travel to the caller's sink
    // (r11w12-tier-helpers-declined-callee-rerun-patch). The drained deopt
    // flag belongs to that trap and is spent with it.
    if signals.exception.is_none()
        && signals.aioobe.is_none()
        && !signals.npe
        && !signals.arithmetic
        && cratonvm_jit::deopt::has_last_deopt()
    {
        if args_owned_by_callee {
            // gcd d9/d: no argument to re-run it with. Drop the stash as the
            // re-run would have, and leave as the caller's frameless deopt.
            if let Some(exact) = drop_declined_callee_stash(vm, thread, info, receiver_class_id) {
                note_callee_owned_args_trap_propagated(info, "declined stash");
                owned_args_propagated = true;
                signals.deopt = true;
                handoff_exact = Some(exact);
            }
        } else if let Some(v) =
            rerun_declined_callee_from_entry(vm, thread, info, receiver_class_id, args_slice, pins)
        {
            return Some(v);
        }
    }
    // The frameless twin of the block above, as the dispatch door's
    // `BARE_CALLEE_TRAPS_SERVICED` arm (round 11 wave 13, applied by lane
    // icwide from lane tier's r11w13-tier-helpers-mic-door-frameless-callee-
    // rerun-patch): a trap in this site's callee that left no frame (its own
    // frameless stub, or `try_resume_trapped_callee`'s GC-drop arm above) is
    // re-run here, instead of reaching the caller as the caller's own
    // frameless deopt, which an OSR'd caller can only answer by replaying
    // every committed iteration. The drained flag is spent with it; it is
    // per-call fresh (`call_compiled_entry_under_owner` retires it before the
    // callee runs), so it is never a stale flag beside a real `Long.MIN_VALUE`.
    if signals.deopt
        && signals.exception.is_none()
        && signals.aioobe.is_none()
        && !signals.npe
        && !signals.arithmetic
        && !cratonvm_jit::deopt::has_last_deopt()
    {
        if args_owned_by_callee {
            // gcd d9/d: no argument to re-run it with; the deopt flag stays
            // set and the tail below re-raises it for the caller's caller.
            if !owned_args_propagated {
                note_callee_owned_args_trap_propagated(info, "frameless trap");
                owned_args_propagated = true;
                // gce e1/f: the callee's own frameless record, read before
                // the tail's re-raise overwrites it.
                handoff_exact = Some(owned_args_frameless_callee_replay_is_exact(
                    last_frameless_trap_site(),
                    info,
                ));
            }
        } else {
            // Round 13 wave 8 (lane replay4): through the dispatch door's
            // service, so this re-run is counted in the door census
            // (`callee-frameless`) and refused when unsound, as there. It was
            // the one call-site re-run from entry nobody counted. The
            // service's own counter bump and flag take are this arm's (the
            // take is a no-op: drained above).
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] frameless callee trap at the MIC door: {}.{}{}",
                    info.class_name, info.method_name, info.descriptor,
                );
            }
            return Some(service_frameless_callee_trap(
                vm,
                thread,
                info,
                receiver_class_id,
                args_slice,
                pins,
            ));
        }
    }
    let throw_pc = if signals.athrow_bci >= 0 {
        signals.athrow_bci as usize
    } else {
        usize::MAX
    };
    // Interpreter round i5 wave 105, lane L2: the pc an EXPLICIT throwable is
    // offered with. Unstamped, it was re-posted by the VM after the callee's
    // bytecode already ran (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`); an implicit
    // signal built here keeps `throw_pc`.
    let explicit_throw_pc = if throw_pc == usize::MAX {
        crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
    } else {
        throw_pc
    };
    // nio21 wave 21 L1: a lambda proxy receiver whose handler-bearing impl
    // left a throwable (explicit, or an implicit signal built here): run the
    // IMPL's table, which the receiver-based probe below cannot find (a proxy
    // has no class-store method). A miss leaves the throwable pending for the
    // restore tail, as a named callee's miss does.
    if !args_owned_by_callee && !(escaped_redispatch && signals.exception.is_some()) {
        let lambda_site = lambda_handler_site_for_service(
            vm,
            info,
            receiver_class_id,
            &args_now(pins, thread, args_slice),
        )
        .filter(|site| !explicit_throwable_already_left_impl(&signals, escape_record, site));
        if let Some(site) = lambda_site {
            let lambda_pc = if signals.exception.is_some() {
                explicit_throw_pc
            } else {
                throw_pc
            };
            let exc = match signals.exception.take() {
                Some(exc) => Some(exc),
                None => {
                    let implicit =
                        implicit_signal_of(signals.aioobe, signals.npe, signals.arithmetic);
                    if implicit == ImplicitSignal::None {
                        None
                    } else {
                        let built = materialize_implicit_signal(
                            vm,
                            thread,
                            implicit,
                            signals.trap_frames.clone(),
                            signals.npe_action,
                        );
                        if built.is_some() {
                            // Consumed: the throwable carries the snapshot.
                            signals.aioobe = None;
                            signals.npe = false;
                            signals.arithmetic = false;
                            signals.trap_frames = None;
                        }
                        built
                    }
                }
            };
            if let Some(exc) = exc {
                // Read AFTER the allocation above, which can collect.
                let handler_args = args_now(pins, thread, args_slice);
                match lambda_route_thrown_through_impl(
                    vm,
                    thread,
                    &site,
                    info,
                    &handler_args,
                    exc,
                    lambda_pc,
                ) {
                    Ok(v) => return Some(v),
                    Err(exc) => signals.exception = Some(exc),
                }
            }
        }
    }
    // Same split as `route_implicit_exc_through_callee`: resolve from the
    // receiver for a virtual/interface site, by name for a statically bound
    // one. Asking the receiver-based probe about an `invokestatic` callee reads
    // arg 0 as a class, which is not one.
    let has_handler = if matches!(info.invoke_kind, 0 | 2) {
        mic_callee_has_exception_table(vm, receiver_class_id, info)
    } else {
        callee_has_exception_table(vm, info)
    };
    // gce e1/f: a throwable that escaped a not-entrant re-dispatch of this
    // very callee has already been offered to its table, by the VM's invoke
    // path, at its real throw pc. The search below would offer it again with
    // the pc unknown, where a typed row matches on class alone and a row the
    // throw had left runs a second time
    // (`gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-DONE-20260928.md`).
    // Propagate it, as a `NotCaught` miss does.
    let has_handler = has_handler && !(escaped_redispatch && signals.exception.is_some());
    // gcd d9/d: an owned-arguments site binds only a callee with no table, so
    // a handler here means the method was redefined with one since. It cannot
    // be run without the arguments: drop the throwable (the caller's re-run
    // raises it again) and leave as the caller's frameless deopt.
    let throwable_pending = signals.exception.is_some()
        || signals.aioobe.is_some()
        || signals.npe
        || signals.arithmetic;
    if args_owned_by_callee && has_handler && throwable_pending {
        if !owned_args_propagated {
            note_callee_owned_args_trap_propagated(info, "callee handler");
        }
        signals.exception = None;
        signals.aioobe = None;
        signals.npe = false;
        signals.arithmetic = false;
        signals.trap_frames = None;
        signals.deopt = true;
    }
    if has_handler && !args_owned_by_callee {
        if let Some(exc) = signals.exception {
            let handler_args = args_now(pins, thread, args_slice);
            match try_run_callee_handler(
                vm,
                thread,
                info,
                receiver_class_id,
                &handler_args,
                exc,
                explicit_throw_pc,
            ) {
                Ok(v) => return Some(v),
                // Restored below. The miss's throwable, not the drained one:
                // it was re-read from a pin, and may be a catch type's
                // resolution error that replaced it.
                Err((_, now, _)) => signals.exception = Some(now),
            }
        } else if throw_pc != usize::MAX
            && implicit_callee_throw_pc_enabled()
            && implicit_throw_is_uncovered(vm, info, receiver_class_id, throw_pc)
        {
            // Round 12 wave 3 (lane exc): no row of the callee covers its
            // stamped throw pc, so `run_jit_callee_handler` would answer
            // `NotCaught` -- after the throwable had been built, only to be
            // discarded and built again by the caller-side drain. Fall through
            // to the restash below instead, which is what that miss did.
        } else {
            // The SAME table the dispatch-helper door uses. This door is where
            // the arithmetic arm always existed; sharing the table is what stops
            // the two from disagreeing about the SET again. See
            // [`ImplicitSignal`].
            // CLONED, not taken: the restash at the end of this function puts
            // the snapshot back when nothing here consumes the throwable, and
            // `take` left it with `None` to put back. Same defect and same fix
            // as the sibling door's.
            let implicit = materialize_implicit_signal(
                vm,
                thread,
                implicit_signal_of(signals.aioobe, signals.npe, signals.arithmetic),
                signals.trap_frames.clone(),
                signals.npe_action,
            );
            if let Some(exc) = implicit {
                // Read AFTER the allocation above, which can collect.
                let handler_args = args_now(pins, thread, args_slice);
                match try_run_callee_handler(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    &handler_args,
                    exc,
                    throw_pc,
                ) {
                    Ok(v) => return Some(v),
                    Err((_, error, true)) => {
                        // A catch type's resolution error replaced the
                        // implicit exception (JVMS §5.4.3). Restore it as a
                        // GENERAL pending exception, as the arm above restores
                        // its miss: restashing the implicit flag and snapshot
                        // below would rebuild the original exception at the
                        // caller and drop the error.
                        signals.exception = Some(error);
                        signals.aioobe = None;
                        signals.npe = false;
                        signals.arithmetic = false;
                        signals.trap_frames = None;
                    }
                    Err(_) => {}
                }
            }
        }
    }

    // Nothing in the callee consumed the exception. Restore precisely the
    // original signal shape so the existing caller-side drain remains the
    // fallback; this preserves behaviour for a miss or a non-covering catch.
    if let Some(exc) = signals.exception {
        set_jit_pending_exception(thread, exc);
    }
    // Restore the snapshot the drain took WITH the flag it belongs to, rather
    // than letting a setter take a fresh one here: `take_all_jit_signals` moved
    // it out, and a second `active_compiled_frames()` at this point describes a
    // shallower stack than the trap did. Without this the caller-side drain
    // builds the throwable from a bare flag and the trace is EMPTY.
    // ONE `trap_frames` slot, so exactly ONE of these three may restore it
    // and the rest come back flag-only. Writing it more than once is not a
    // tidiness point: every one of these setters ASSIGNS the slot, so a
    // second call with `None` would wipe the frames the first put back.
    // JVMS does not let one frame have two of these pending at once, so
    // the priority never arbitrates anything real -- it is written out so
    // the next reader does not have to derive that from three `take()`s.
    let frames = signals.trap_frames.take();
    if signals.npe {
        restash_jit_pending_npe(signals.npe_action, frames);
        if signals.arithmetic {
            stash_jit_pending_arithmetic();
        }
        if let Some((index, length)) = signals.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if signals.arithmetic {
        restash_jit_pending_arithmetic(frames);
        if let Some((index, length)) = signals.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if let Some((index, length)) = signals.aioobe {
        restash_jit_pending_aioobe(index, length, frames);
    }
    // JIT round 17 wave 4, lane stashdrop (site 5 of
    // `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
    // the callee threw and its exception leaves this door (a `Declined` /
    // `Unknown` miss, which includes a `resolve_callee_cached` failure, an
    // `escaped_redispatch`, or an owned-arguments hand-off). Its activation
    // has unwound and no sink will ever resume the frame it published, so
    // under the exceptional hand-over's switches a top frame naming it is
    // taken and what it names released
    // (`CompiledLocksOfAStash::release_for_a_dropped_frame`), as
    // `route_implicit_exc_through_callee` does through
    // `clear_own_exceptional_frame`. Left stashed, it ended under the caller's
    // own frame and was discarded unreleased at the next take. A `NotCaught`
    // miss already took and released it. Nothing here allocates: the restored
    // throwable stays valid. Off, the frame is left for the caller as before.
    //
    // JIT round 19 wave 4, lane syncdoor4 (SELFLOCK19-1 of
    // `jit-r19-selflock19-proposals.md`): the stash is asked BEFORE the two
    // switches (each an env read). It is empty for almost every exception that
    // leaves here, so most escapes now read no flag. The same answer: all three
    // questions are pure, and the drop needs every one of them.
    if throwable_pending {
        if let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() {
            if exceptional_frame_key_names(&key, info.method_name, info.descriptor)
                && crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released(
                )
            {
                let _ = drop_exceptional_frame_of(vm, thread, &key, bci, "MIC door miss");
            }
        }
    }
    if signals.deopt {
        set_jit_deopt_pending();
        // gce e1/f: an owned-arguments hand-off tells the sink that re-runs
        // the caller what that re-run replays (after the re-raise above,
        // which records `Helper`).
        if let Some(exact) = handoff_exact {
            note_owned_args_handoff(exact);        }    }
    None
}

/// gce e1/f (`gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md`,
/// the census residual): record that the deopt flag just raised is an
/// owned-arguments hand-off whose caller re-run replays `exact`ly
/// ([`FramelessTrapSite::Handoff`]). Call right after
/// [`set_jit_deopt_pending`]. `CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT=0`
/// leaves the `Helper` record, i.e. the old whole-body judgement.
pub fn note_owned_args_handoff(exact: bool) {
    // gcf f3/f: counted whatever the switch says, for the one-line exit
    // report (`deopt_resume::owned_args_handoff_census`), so a row's
    // engagement is read from the end of its stderr.
    crate::runtime::interpreter::note_door_rerun(
        crate::runtime::interpreter::DoorRerunCause::OwnedArgsHandoff,
        exact,
    );
    {
        JIT_SIGNALS.with(|s| s.frameless_trap.set(FramelessTrapSite::Handoff { exact }));
    }
}

/// gce e1/f: the replay verdict of a caller re-run after an owned-arguments
/// site handed off its callee's FRAMELESS trap. The caller's own prefix is
/// exact by the site's admission (`x64/op_invoke.rs`,
/// `owned_args_handoff_replays_exactly`), so it is the callee's replay: the
/// trapping stub's stamp when the record names this very callee, or the
/// dropped frame's verdict; anything else is not exact (the old answer).
pub fn owned_args_frameless_callee_replay_is_exact(
    site: FramelessTrapSite,
    info: &JitInvokeInfo,
) -> bool {
    match site {
        FramelessTrapSite::Stub { exact, method, .. } => {
            exact
                && method != 0
                && method
                    == frameless_trap_method_hash(
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                    )
        }
        FramelessTrapSite::Dropped { exact } => exact,
        FramelessTrapSite::Handoff { .. }
        | FramelessTrapSite::Helper
        | FramelessTrapSite::Unknown => false,
    }
}

/// `CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT` (JIT round 16 wave 15, lane trapfix;
/// **default ON**, `=0` restores the round-16 base): does a deoptimization that
/// evicts a method's published body
/// (`cratonvm_jit::tiered::deopt_evicts_method_body`) also make that body not
/// entrant ([`make_trap_evicted_body_not_entrant`]), and does the call-site
/// service leave uncharged a trap whose method an earlier trap already
/// declared not compilable ([`trap_from_a_body_already_declared_not_compilable`])?
///
/// The defect it closes
/// (`r16-framestate-spliced-callee-trap-repeats-after-make-not-compilable-FIXED-20261002.md`):
/// the eviction withdrew the body from the cache, the inline caches and the
/// published raw callers, but a caller frame ALREADY RUNNING a loop kept the
/// evicted body's address in its baked `CALL` ([`despeculate_trapped_method`]'s
/// doc measured it on `UnresolvedTrapProbe`). Each failing call then trapped in
/// the evicted body and, the slot being empty, was charged again: ~6 700
/// `MakeNotCompilable` decisions in `R16FramestateChainCoverage`, one per
/// failing call. Read per eviction and per uncharged-candidate trap, both cold.
pub fn trap_eviction_not_entrant_enabled() -> bool {
    true
}

/// Make `evicted`, the method-entry body a deoptimization just withdrew
/// (`DeoptimizationController::deoptimize_in`'s `remove`), not entrant: its
/// entry pad becomes a `JMP` to the not-entrant stub
/// (`jit/src/not_entrant.rs`), so a caller that baked its address -- above
/// all a caller frame already running when the eviction happened, which no
/// invalidation can re-bind -- re-dispatches the call instead of entering the
/// withdrawn code: forwarded to the method's current body once one is
/// published (`jit_not_entrant_entry`'s forward word), interpreted while none
/// is (a `MakeNotCompilable` method is then simply interpreted, and its
/// failing guard costs an interpreted throw, not a deoptimization). Frames
/// already inside the body finish in it.
///
/// The eviction flavour of the pass (`JitCache::make_not_entrant_after_eviction`):
/// the body runs current bytecode, so it is not marked withdrawn by a
/// redefinition. The hook is armed exactly as the redefinition, the VarHandle
/// mint and the late-bind retire arm it; idempotent. A refusal (no entry pad,
/// an OSR body, an arity the record cannot name) changes nothing: the
/// call-site service's once-only charge then still bounds the cost
/// ([`trap_from_a_body_already_declared_not_compilable`]).
///
/// `evicted` is released through `RetainedCode`, not dropped bare: it may be
/// the body's last owner once the retirement queue drained its own reference,
/// and a bare drop would free published code outside an authorised
/// reclamation (`published_code_free_audit`).
pub fn make_trap_evicted_body_not_entrant(
    vm: &SharedVm,
    method_key: &str,
    evicted: std::sync::Arc<cratonvm_jit::CompiledMethod>,
) {
    let evicted = cratonvm_jit::RetainedCode::new(evicted);
    vm.jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: vm as *const SharedVm as usize,
        });
    let patched = vm
        .jit
        .jit_cache
        .make_not_entrant_after_eviction(std::slice::from_ref(evicted.arc()));
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {method_key}: evicted body id={} made not entrant (patched={patched})",
            evicted.compile_id
        );
    }
}

/// t1 wave 6 (lane J6): arm this VM's not-entrant hook at startup when
/// `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT` (opt-in, t1 wave 5 lane X2;
/// `cratonvm_jit::jit_invalidate_not_entrant_enabled`) is on. Called by
/// `Vm::new` once the `Arc<SharedVm>` exists (the hook carries the VM's
/// address, stable only then; `SharedVm::new` cannot arm it).
///
/// Without it the switch was inert until something else armed the cache: the
/// first trap eviction, redefinition, VarHandle mint, late-bind retire or
/// MH-constant retire. A dependency invalidation before that (a class load
/// that breaks a CHA assumption, the common case) withdrew its bodies and
/// patched none, so an optimizing caller's baked `CALL` kept entering them;
/// whether a run patched depended on event order. Armed here, every
/// invalidation of the run is covered.
///
/// Per VM (this VM's cache, this VM's address), idempotent like every other
/// arming site. Arming also engages, from startup, what the other arming
/// sites engage from their first event: `JitCache::put`'s supersede redirect
/// and the not-entrant halves of the VarHandle mint / late bind (the effect
/// `CRATONVM_JIT_ARM_NOT_ENTRANT_AT_PUBLISH` measured neutral in round 20).
/// Returns whether it armed.
pub fn arm_not_entrant_for_dependency_invalidation(vm: &SharedVm) -> bool {
    if !cratonvm_jit::jit_invalidate_not_entrant_enabled() {
        return false;
    }
    vm.jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: vm as *const SharedVm as usize,
        });
    true
}


/// Is a trap the call-site service is about to charge one that an earlier
/// trap already charged to the end (JIT round 16 wave 15, lane trapfix;
/// FS14-4 of `jit-r16-framestate-proposals-RETIRED-20261006.md`)? `true` when the cache
/// publishes no body for the method and this VM's runtime de-speculation
/// registry holds a current `MakeNotCompilable` verdict for it
/// (`RuntimeDespecRegistry::is_not_compilable`, which a redefinition or a
/// code-cache flush expires): the trap came out of a body that verdict
/// evicted, through a binding the eviction could not withdraw. Charging it
/// again records another deoptimization, re-decides `MakeNotCompilable`,
/// re-writes the skip set and the registry and re-counts the per-bci de-spec,
/// with no compile left to shape. The resume itself still runs.
///
/// Under `CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT` ([`trap_eviction_not_entrant_enabled`]).
pub fn trap_from_a_body_already_declared_not_compilable(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    class_id: ClassId,
    bci: u32,
) -> bool {
    let decided = trap_eviction_not_entrant_enabled()
        && vm
            .jit
            .jit_cache
            .get(class_name, method_name, descriptor, class_id)
            .is_none()
        && vm
            .jit
            .runtime_despec
            .is_not_compilable(class_id, class_name, method_name, descriptor);
    if decided && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {class_name}.{method_name}:{descriptor} bci={bci}: trap from a body \
             already declared not compilable, not charged"
        );
    }
    decided
}


/// De-speculate a method whose compiled body just trapped — TAKING THE
/// SITE-TRAP DECISION ONCE.
///
/// `fallback_reason` is what to record when this is NOT an IR site trap; the
/// two callers pass different ones because they know different things.
///
/// # Why the two consumers of a stashed deopt frame must ask the same function
///
/// A trapped compiled body's frame reaches one of two sinks, depending on how
/// the callee was entered: `try_resume_trapped_callee` (the compiled caller's
/// dispatch helper, for a callee that already has an artifact) or `execute`'s
/// first-call tier-up sink (`interpreter.rs`, `execute-first-call-tierup`, for
/// the invocation that installs one). They disagreed. The helper applied the
/// policy below; the tier-up sink passed the DEOPT POINT'S OWN reason straight
/// to `deoptimize`, and for a body whose trapping bci carries a safepoint that
/// reason is `TransferToInterpreter`, whose `recommend_action` is `Reinterpret`
/// — the artifact stays live, the next call re-enters it, and it traps again,
/// forever.
///
/// # The policy
///
/// `UnreachedCode` (the usual fallback) is the one-shot "give up immediately"
/// reason: the trapping method is made not-compilable on the first resolution
/// and the tiered manager stops re-queuing recompiles that would just trap.
///
/// EXCEPT for an IR SITE TRAP. `probes/UnresolvedTrapProbe.java` measures why:
/// the trapped method deopts on EVERY call (200,000 of 200,000) and
/// `MakeNotCompilable` is consulted by `compile_gate` itself, so the method
/// loses its body on EVERY tier — including the single-pass backend, which
/// lowers `invokedynamic` and unresolved typechecks perfectly well. Before site
/// traps existed the IR tier simply refused such a method and C1 compiled it;
/// blacklisting is a strict regression on that.
///
/// A site trap means one thing only: the OPTIMIZING tier could not lower one
/// call site. So ban the optimizing tier for this method — the memo
/// `try_compile_inner` already consults — and pick a reason that RECOMPILES
/// instead of blacklisting. The recompile then goes single-pass and does not
/// trap. `SpeculationFailed` is that reason: `RecompileAndReinterpret` until the
/// per-method deopt count crosses `max_deopts_per_method`, which keeps a
/// backstop if the assumption above is ever wrong.
///
/// # Why the decision is taken ONCE, and in its OWN set
///
/// Everything after the first is a REPEAT of a decision already taken, and
/// repeating it is not harmless: `SpeculationFailed` escalates on the
/// per-method deopt COUNT, so a trapped site inside a long-running caller drives
/// the method to `MakeNotCompilable` — the exact outcome this arm exists to
/// avoid — purely by being reached often.
///
/// Why "often" is unavoidable: eviction and the epoch bump both fire on the
/// first deopt, and `invalidate_matching` even evicts the direct CALLER
/// transitively. But the caller may be a single in-flight invocation running a
/// loop — measured on `probes/UnresolvedTrapProbe.java`, where one `main` frame
/// calls the trapped callee 200,000 times through a `CALL` baked into code that
/// is already executing. Re-binding happens (the tracer shows `main` recompiled
/// and baking a second callee entry) but the RUNNING frame keeps the old
/// address, and nothing short of deoptimizing the caller's frame can change
/// that. `MakeNotEntrant` is an enum variant here with no entry-patching behind
/// it, so there is no cheap displacement to reach for.
///
/// What IS in reach is not compounding the damage: take the policy decision
/// once, and let the remaining calls resume in the interpreter — correct, and
/// self-correcting the moment that caller frame returns and re-enters its
/// recompiled self.
///
/// "Once" is `JitVerdictRegistry::claim_site_trap_decision`, a set of its own (per VM, per declaring class, per redefine epoch), and not
/// `ir_evidence`'s refusal memo — see 81c9c9fd7 and the comment at the call.
///
/// Round 16 wave 15 (lane trapfix): `MakeNotEntrant` now has entry patching
/// behind it. Every evicting deoptimization makes the evicted method-entry
/// body not entrant ([`make_trap_evicted_body_not_entrant`]), so the running
/// caller's baked `CALL` reaches the not-entrant stub, which forwards to the
/// recompiled body or re-dispatches through the interpreter.
pub fn despeculate_trapped_method(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
) {
    despeculate_trapped_method_inner(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        None,
        false,
        None,
    );
}

/// `CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD` -- JIT round 19 wave 12, lane
/// tail12 (`r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
/// **Default ON**, `0` (kill word) restores the old record: with
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` off, an inlined CHAIN's
/// per-bci de-spec at the helper and first-call sinks records the WILDCARD at
/// the call site (counted per `(reason, call-site bci)`, as before), exactly as
/// the door sink's chain arm records it, instead of the reason's named id there
/// -- which no consumer reads, so a spliced guard that kept failing was spliced
/// again by every recompile until the aggregate per-method count retired the
/// method. Read per trapped-frame despeculation (cold); not cached.
pub fn helper_chain_site_wildcard_enabled() -> bool {
    true
}

/// JIT round 19 wave 4 (lane splice4, SPLICE3-2): [`despeculate_trapped_method`]
/// for a sink that holds the stashed frame `rframe` the trap is charged for.
/// Under `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` (with
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW`), an inlined CHAIN's per-bci
/// escalation goes through `deopt_resume::despec_chain_site_after_trap_limit`
/// (an implicit exception names the innermost guard first, as the door sink
/// does since wave 3) instead of recording the call-site entry at once, and
/// the call-site entry it records is the wildcard, as the door sink's chain
/// arm records it (off the switch this sink records the reason's named id at
/// the call-site bci, which no consumer reads, so a chain-trapping splice is
/// never withdrawn here: `r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
/// Switch off, or a single-scope frame: exactly [`despeculate_trapped_method`].
/// The caller has already asked `deopt_resume::stash_sink_expected_throw_uncharged`
/// and skipped this call for a marked trip it leaves uncharged.
#[allow(clippy::too_many_arguments)]
pub fn despeculate_trapped_method_of_frame(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) {
    despeculate_trapped_method_of_frame_despec_as(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        rframe,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn despeculate_trapped_method_inner(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    chain: Option<&cratonvm_jit::deopt::ReconstructedFrame>,
    chain_site_wildcard: bool,
    despec_as: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    // Interpreter round i1 wave 15, lane L3: a method-entry body that left at
    // a back-edge poll because the method must now run interpreted did not
    // mis-speculate. No charge, no site-trap memo, no per-bci de-spec
    // (`interpreter::exit_left_for_the_interpreter`).
    if crate::runtime::interpreter::exit_left_for_the_interpreter(
        vm,
        fallback_reason,
        declaring_class_id,
        method_name,
        descriptor,
    ) {
        return;
    }
    // JIT round 19 wave 3, lane eager3 (review of EAGER19-1): an
    // uninitialized-`new` trap is a SITE trap (the builder lists its pc), so
    // the policy below would ban the optimizing tier for the method (sticky
    // under the default evidence policy) and charge `SpeculationFailed`, which
    // `deoptimize_in`'s hook does not serve: the late-bind recompile the trap
    // exists to buy was never asked, and the method stayed single-pass. Charge
    // it as itself instead (`jit_bridge::charge_uninit_new_trap`). Engages only
    // at a site `CRATONVM_JIT_UNINIT_NEW_TRAP` / `_GUARD` admitted (one lock
    // when that map is non-empty), so unconditional.
    if fallback_reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap
        && bci != u32::MAX
        && crate::runtime::interpreter::jit_bridge::uninit_new_trap_site_known(
            vm,
            declaring_class_id,
            method_name,
            descriptor,
            bci,
        )
    {
        crate::runtime::interpreter::jit_bridge::charge_uninit_new_trap(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            bci,
            false,
        );
        return;
    }
    // Cheap negative first: both sinks call this on EVERY trapped-frame
    // despeculation, and a VM that planted no site trap at all must not pay a
    // hash and a lock read for it. Doubly so now that site traps are opt-in.
    // This VM's site-trap memo and refusal memo, keyed by the declaring class
    // id (and the decision by this VM's redefine epoch): interpreter round i1
    // wave 23, `i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`.
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    let (ir_site_trap, h) = if verdicts.any_site_trap_registered() {
        let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
        let method_level = verdicts.method_has_site_trap(h, declaring_class_id);
        (
            site_trap_policy_applies(
                verdicts,
                h,
                declaring_class_id,
                bci,
                method_level,
                (class_name, method_name, descriptor),
            ),
            h,
        )
    } else {
        (false, 0)    };
    // JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
    // a planted `invokedynamic` site trap whose instruction the linked bridge
    // serves once the interpreter has linked it (it was unlinked when the body
    // was built) is a late bind, not a site this tier cannot lower: recompile
    // without the IR ban (`jit_bridge::indy_late_link_trap_reprieve`).
    if ir_site_trap
        && chain.is_none()
        && bci != u32::MAX
        && verdicts.method_has_site_trap_at(h, declaring_class_id, bci)
        && crate::runtime::interpreter::jit_bridge::indy_late_link_trap_reprieve(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            bci,
        )
    {
        DeoptimizationController::deoptimize_or_by_name(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
            bci,
        );
        return;
    }
    let mut decided = true;
    if ir_site_trap {
        cratonvm_jit::ir::note_site_trap_taken();
        // A DEDICATED set, not `ir_evidence`'s refusal memo (81c9c9fd7): that
        // memo has a second writer -- the acceptance gate marks a method
        // refused whenever it discards an optimizing body -- so reading it here
        // would let a gate-refused method look "already decided" on its FIRST
        // trap, and the policy, including the eviction, would never be applied.
        // The IR ban goes under the compile door's own key (method, declaring
        // class id, redefine epoch); a repeat re-decides while that ban could
        // not be recorded (JIT round 21, lane lambdatrap). j27 wave 7, lane
        // T1LAMTRAP (LAMBDATRAP21-1): the one claim/ban step all three
        // site-trap-style policies share, `claim_site_trap_policy`.
        decided = claim_site_trap_policy(
            verdicts,
            h,
            (class_name, method_name, descriptor),
            declaring_class_id,
            SiteTrapPolicy::SiteTrap,
        );
    }
    if decided {
        // The caller's declaring class id, not a by-name re-lookup: with two
        // loaders defining `class_name` the lookup answers id 0 and the
        // tiered manager's key misses the real state
        // (r11-tier-deopt-tiering-key-drops-the-class-id). Id 0 (unit tests,
        // an unresolved caller) keeps the by-name route.
        DeoptimizationController::deoptimize_or_by_name(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            if ir_site_trap {
                cratonvm_jit::deopt::DeoptReason::SpeculationFailed
            } else {
                fallback_reason
            },
            bci,
        );
        // Per-bci de-spec, the rule `real_frame_deopt_resume_and_despeculate`
        // (`deopt_resume.rs`) applies. It lives here too because a method whose
        // trap arrives through a COMPILED caller never reaches that sink: its
        // failing loop-header speculative-BCE guard was re-emitted on every
        // recompile until the aggregate count retired the whole method
        // (`TryLoop`: 18.9 s vs HotSpot's 16 ms; round 9 wave 8, review8c).
        // `deopt_count_at_bci` counts charged traps only, so an exception-only
        // bci never de-specs. `deoptimize` has released the log lock by now
        // (`record_deoptimization` scopes it), so re-locking cannot deadlock.
        // The rule itself (limit, grain, sentinel, insert) is
        // `deopt_resume::despec_site_after_trap_limit`'s (round 14 wave 4,
        // lane resume2, RS-3); this site keeps its choice of grain and id.
        if bci != u32::MAX {
            let method_key = format!("{class_name}.{method_name}:{descriptor}");
            // `trapped_callee_deopt_reason` answers `UncommonTrap` exactly
            // when the reason at this bci could not be read uniquely; record
            // the wildcard then, as the other sink does for `Ambiguous`/`None`.
            let wildcard =
                ir_site_trap || fallback_reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap;
            // Count at the grain of the verdict (HotSpot's
            // `PerBytecodeTrapLimit` is per `(bci, reason)`): three traps of
            // one kind and one of another at a bci must not withdraw the
            // speculation that failed once. The wildcard withdraws everything
            // at the bci, so it keeps the whole-bci sum. Same rule as
            // `deopt_resume::real_frame_deopt_resume_and_despeculate`
            // (r11w4-tier-helpers-per-bci-despec-limit-sums-every-reason).
            // nio21 wave 7 (L2): a caller-named attribution (an inlined
            // chain's, [`chain_trap_charge`]) is the doors' grain: counted at
            // its reason, withdrawn under its id. JIT round 19 wave 12, lane
            // tail12: otherwise a chain's call-site record is the wildcard
            // (`CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD`), still counted at
            // the reason's grain.
            let (counted, speculation) = match despec_as.filter(|_| !ir_site_trap) {
                Some(c) => (Some(c.reason), c.speculation_id),
                None if wildcard => (None, cratonvm_jit::deopt::SPECULATION_ID_ANY),
                None if chain_site_wildcard => (
                    Some(fallback_reason),
                    cratonvm_jit::deopt::SPECULATION_ID_ANY,
                ),
                None => (
                    Some(fallback_reason),
                    cratonvm_jit::deopt::speculation_id(bci, fallback_reason),
                ),
            };
            // JIT round 19 wave 4 (lane splice4, SPLICE3-2): a chain's
            // implicit exception names its innermost guard first
            // (`despeculate_trapped_method_of_frame`; `None` everywhere else).
            // Its call-site record is the WILDCARD, as the door sink's chain
            // arm records it: the planner withdraws a splice only for the
            // wildcard at its invoke (`ir_splice_site_withdrawn`), so the named
            // `speculation_id(call-site bci, reason)` this sink records for a
            // chain off the switch withdraws nothing (see
            // `r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
            let withdrawn = match chain {
                Some(rframe) if !ir_site_trap => {
                    crate::runtime::interpreter::despec_chain_site_after_trap_limit(
                        vm,
                        &method_key,
                        rframe,
                        bci,
                        counted,
                        cratonvm_jit::deopt::SPECULATION_ID_ANY,
                    )
                }
                _ => crate::runtime::interpreter::despec_site_after_trap_limit(
                    vm,
                    &method_key,
                    bci,
                    counted,
                    speculation,
                ),
            };
            if let Some(bci_deopts) = withdrawn {
                if dbg_deopt_on() {
                    eprintln!(
                        "[cratonvm-deopt] per-bci de-spec (trapped callee): {method_key} \
                         bci={bci} ({bci_deopts} deopts >= {})",
                        crate::runtime::interpreter::PER_BCI_DESPEC_LIMIT
                    );
                }
            }
        }
    }
}

/// `CRATONVM_JIT_SITE_TRAP_PER_BCI` (JIT round 19 proposal wave 7, lane
/// interp7; EAGER3-19-1 / OSRENTRY6-1 of `jit-r19-eager3-proposals.md` /
/// `jit-r19-osrentry6-proposals.md`; **default OFF**, `1` enables): see
/// [`site_trap_policy_applies`]. Read per trapped-frame despeculation (cold).
///
/// Placed AFTER [`despeculate_trapped_method_inner`] on purpose: the source
/// witness `r19w3_eager3_tests::the_site_trap_policy_charges_an_uninit_new_trap_as_itself`
/// reads a fixed window from `despeculate_trapped_method`.
pub fn site_trap_per_bci_enabled() -> bool {
    true
}

/// Does the site-trap policy (IR ban, `SpeculationFailed`, the wildcard
/// per-bci de-spec) apply to a trap of the method `h` / `class_id` at `bci`?
///
/// `method_level` is the per-method registration
/// (`JitVerdictRegistry::method_has_site_trap`): some optimizing body of the
/// method once planted a site trap SOMEWHERE. That key is shared by the
/// method-entry body and every OSR body and is never withdrawn (an older
/// trapping body may still run), so after a recompile publishes a body with no
/// site trap, a genuine uncommon trap at an unrelated bci still took the
/// policy (EAGER3-19-1). Under the switch, a known `bci` takes the policy only
/// when a body planted a site trap AT that bci
/// (`JitVerdictRegistry::method_has_site_trap_at`, registered beside the
/// per-method key); any other bci is an ordinary uncommon trap, the route of
/// every method that never planted one. An unknown bci (`u32::MAX`) keeps the
/// per-method answer.
///
/// Diagnostic (both arms, `CRATONVM_DBG_JITC`): EAGER3-19-1's first-step
/// count, one line per policy decision at a bci that holds no planted trap.
pub fn site_trap_policy_applies(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    class_id: ClassId,
    bci: u32,
    method_level: bool,
    names: (&str, &str, &str),
) -> bool {
    if !method_level || bci == u32::MAX {
        return method_level;
    }
    if verdicts.method_has_site_trap_at(h, class_id, bci) {
        return true;
    }
    let per_bci = site_trap_per_bci_enabled();
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] site-trap policy at a bci with no planted site trap: {}.{}{} bci={bci} ({})",
            names.0,
            names.1,
            names.2,
            if per_bci {
                "ordinary uncommon trap under CRATONVM_JIT_SITE_TRAP_PER_BCI"
            } else {
                "policy applied by the per-method key"
            },
        );
    }
    !per_bci
}

/// Zero-divisor exits of one compiled `(method, bci)` tolerated before the
/// method is re-tiered — see [`retier_hot_division_guard_trap`]. HotSpot's
/// `PerBytecodeTrapLimit` is 4; 16 is this VM's own choice (it once matched
/// `FAST_THROW_HOT_THRESHOLD`, which has been 10 000 since nio21 wave 16 --
/// comment corrected JIT round 22 wave 2, lane throws, L5W16-4), so a
/// method that divides by zero a handful of times keeps its optimizing body.
pub const DIVISION_GUARD_RETIER_THRESHOLD: u32 = 16;

/// Bound on the per-thread table below; a full table is cleared (counting
/// restarts — slower, never wrong).
pub const DIVISION_GUARD_SITE_CAP: usize = 1024;

/// `(vm identity, ir_method_memo_hash, declaring class id, the VM's redefine epoch,
/// bci)`. The class id and epoch since interpreter round i1 wave 23: the
/// counter fired once per key (since round 21: at the threshold, then at
/// powers of two), so a key by names alone let a second
/// loader's copy, or the redefined version, of a method count past the
/// threshold without ever re-tiering
/// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`).
pub type DivisionGuardSiteKey = (usize, u64, u32, u32, u32);

thread_local! {
    /// Zero-divisor guard exits counted per site, per thread. Per thread for
    /// the reason `exceptions.rs`'s fast-throw table is: no lock on a path
    /// that is already paying a deopt, and a threshold per thread only delays
    /// the decision on a multi-threaded program, it never changes it.
    pub static DIVISION_GUARD_TRAPS: std::cell::RefCell<rustc_hash::FxHashMap<DivisionGuardSiteKey, u32>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `idiv`, `ldiv`, `irem`, `lrem` — the four bytecodes a zero divisor traps.
pub fn is_integer_division_opcode(op: u8) -> bool {
    matches!(op, 0x6c | 0x6d | 0x70 | 0x71)
}

/// Count one guard exit at `key`; `true` on the exit that reaches
/// `threshold`, and again on every later exit whose count is a power of two
/// (JIT round 21, lane smallfix, REVVM221-1; correctness of the re-decide,
/// unconditional). Firing once only meant the same thread and site never
/// asked `site_trap_policy_must_apply` again, so a repeat whose IR ban could
/// not be recorded (refusal memo at its bound) kept the rebuilt optimizing
/// body for good. Bounded: at most ~32 fires per key over a `u32` count
/// (`u32::MAX`, where the count saturates, is not a power of two), and each
/// later fire is a claim-set no-op while the ban holds. Pure over `counts`.
pub fn note_division_guard_trap<K: std::hash::Hash + Eq>(
    counts: &mut rustc_hash::FxHashMap<K, u32>,
    key: K,
    threshold: u32,
    cap: usize,
) -> bool {
    if counts.len() >= cap && !counts.contains_key(&key) {
        counts.clear();
    }
    let n = counts.entry(key).or_insert(0);
    *n = n.saturating_add(1);
    *n == threshold || (*n > threshold && n.is_power_of_two())
}

/// Should a precise-resumed trap be counted as a zero-divisor guard exit of
/// the OPTIMIZING tier? Pure, for the test.
///
/// The stash names the division's own bci and the artifact's point there is
/// the plain `TransferToInterpreter` resume point: that is the IR tier's
/// anchored `Guard` for `ArithmeticException` (`ir_lower.rs`
/// `emit_div_zero_guard`, "the ArithmeticException is that guard's job"),
/// which deopts so the interpreter re-executes the division and throws. The
/// single-pass tier never stashes a frame here — its zero check calls
/// `jit_throw_arithmetic` and throws without leaving the caller's compiled
/// code — so nothing else can produce this pair.
pub fn is_division_guard_trap(reason: cratonvm_jit::deopt::DeoptReason, opcode: Option<u8>) -> bool {
    reason == cratonvm_jit::deopt::DeoptReason::TransferToInterpreter
        && opcode.is_some_and(is_integer_division_opcode)
}

/// r9w10 exc10 — stop a hot zero divisor from deoptimizing its callee on
/// every throw.
///
/// `perf-implicit-exceptions-in-compiled-code-cost-microseconds-each`: with
/// `ExcLoop.div` compiled by the optimizing tier, EVERY `k / 0` is
/// `reason=TransferToInterpreter action=Reinterpret` plus a helper
/// precise-resume (199 378 deopts for 198 500 throws on `callee 200000`,
/// `CRATONVM_DBG_DEOPT`), ~4.1 us each, flag or no flag. `Reinterpret` keeps
/// the artifact, `TransferToInterpreter` is never charged, so the per-bci
/// de-spec above never fires either: nothing ever changes, and the callee
/// traps forever. When `div` stays on the single-pass tier the same throw is
/// `jit_throw_arithmetic` + the caller's drain, ~1.5 us (fast throw on) /
/// ~3.2 us (off) on the same run.
///
/// So after [`DIVISION_GUARD_RETIER_THRESHOLD`] such exits at one site, apply
/// the SITE-TRAP policy [`despeculate_trapped_method`] documents, for the
/// same reason it exists: the optimizing tier cannot lower this site without
/// a deopt, and the single-pass tier can. Ban the IR tier for the method (the
/// memo `try_compile_inner` consults) and deoptimize with
/// `SpeculationFailed`, which evicts and recompiles — single-pass, whose zero
/// check throws directly. Claimed ONCE per method through the same
/// `claim_site_trap_decision` set, so the repeats a long-running caller's baked
/// `CALL` still produces cannot escalate the method to not-compilable.
///
/// Kill switch: `CRATONVM_JIT_DIV_GUARD_RETIER=0` (default on).
#[allow(clippy::too_many_arguments)]
pub fn retier_hot_division_guard_trap(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    reason: cratonvm_jit::deopt::DeoptReason,
    opcode: Option<u8>,
    bci: u32,
) {
    if !is_division_guard_trap(reason, opcode) || !division_guard_retier_enabled() {
        return;
    }
    let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
    let key: DivisionGuardSiteKey = (
        vm.vm_identity,
        h,
        declaring_class_id.as_u32(),
        vm.jit.jit_cache.redefine_epoch(),
        bci,
    );
    let reached = DIVISION_GUARD_TRAPS.with(|traps| match traps.try_borrow_mut() {
        Ok(mut counts) => note_division_guard_trap(
            &mut counts,
            key,
            DIVISION_GUARD_RETIER_THRESHOLD,
            DIVISION_GUARD_SITE_CAP,
        ),
        Err(_) => false,
    });
    if !reached {
        return;
    }
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    // JIT round 21 (lane revvm2, LAMBDATRAP21-1): a repeat re-decides when the
    // IR ban could not be recorded (`claim_site_trap_policy`, which also
    // records the ban and charges a repeat).
    if !claim_site_trap_policy(
        verdicts,
        h,
        (class_name, method_name, descriptor),
        declaring_class_id,
        SiteTrapPolicy::DivisionGuard,
    ) {
        return;
    }
    let action = DeoptimizationController::deoptimize_or_by_name(
        vm,
        declaring_class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        bci,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] division-guard re-tier: {class_name}.{method_name}:{descriptor} \
             bci={bci} ({DIVISION_GUARD_RETIER_THRESHOLD} zero-divisor exits) -> IR tier \
             refused, action={action:?}"
        );
    }
}

/// JIT round 21 (lane revvm2, LAMBDATRAP21-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`; correctness of
/// routing, unconditional): must a site-trap-style re-tier
/// ([`retier_hot_division_guard_trap`], [`retier_hot_optimizing_catch`]) apply
/// its policy now? Yes when this is the method's first decision
/// (`claim_site_trap_decision`), and ALSO when a later one finds the IR ban
/// not in effect (`ir_method_ban_holds`: the refusal memo was at its bound
/// when the first decision tried to record it). Then the recompile the first
/// decision asked for rebuilt the same optimizing body, and returning on the
/// claim alone left it installed for good -- the hole round 21 closed in
/// [`despeculate_trapped_method_inner`] only. The re-decision is bounded as
/// that one is: `SpeculationFailed`'s per-method escalation.
///
/// t1 wave 8, lane LAMBDATRAP: a repeat records the ban itself before asking
/// whether it holds, as [`despeculate_trapped_method_inner`] does. Otherwise a
/// repeat on another thread that lands between the first decision's claim and
/// its `note_ir_method_refused` saw "no ban" and applied the policy a second
/// time (one more `SpeculationFailed` charge toward not-compilable, one more
/// eviction). The record is idempotent; at the memo's bound it still fails,
/// which is the case the re-decision is for.
pub fn site_trap_policy_must_apply(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    (class_name, method_name, descriptor): (&str, &str, &str),
    declaring_class_id: ClassId,
) -> bool {
    if verdicts.claim_site_trap_decision(h, declaring_class_id) {
        return true;
    }
    verdicts.note_ir_method_refused(class_name, method_name, descriptor, declaring_class_id);
    !verdicts.ir_method_ban_holds(class_name, method_name, descriptor, declaring_class_id)
}

/// The three site-trap-style policies that share one per-method decision
/// ([`claim_site_trap_policy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteTrapPolicy {
    /// A planted IR site trap ([`despeculate_trapped_method_inner`]).
    SiteTrap,
    /// A hot zero-divisor guard ([`retier_hot_division_guard_trap`]).
    DivisionGuard,
    /// Hot catches outside compiled code ([`retier_hot_optimizing_catch`]).
    HotCatch,
}

impl SiteTrapPolicy {
    /// The `policy` word of the `site-trap repeat` census line.
    pub fn census_name(self) -> &'static str {
        match self {
            SiteTrapPolicy::SiteTrap => "site-trap",
            SiteTrapPolicy::DivisionGuard => "division-guard",
            SiteTrapPolicy::HotCatch => "hot-catch",
        }
    }

    /// Whether a repeat charges the process-wide `note_site_trap_repeat`
    /// counter. A hot catch is not a trap, so it never did.
    pub fn charges_site_trap_repeat(self) -> bool {
        self != SiteTrapPolicy::HotCatch
    }
}

/// j27 wave 7, lane T1LAMTRAP (LAMBDATRAP21-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the
/// ONE claim/ban step of the three site-trap-style policies, which each did it
/// by hand (`despeculate_trapped_method_inner` inline, the two re-tiers through
/// [`site_trap_policy_must_apply`]). Answers whether the caller applies its
/// policy (deoptimize with `SpeculationFailed`) now: the method's first
/// decision, or a repeat that finds no IR ban in effect. Either way the IR ban
/// is recorded under the compile door's own key (method, declaring class id,
/// redefine epoch; idempotent). A repeat that stands down charges the repeat
/// counter (not for a hot catch) and the per-method census line
/// ([`note_site_trap_repeat_census`]). Behaviour is the three callers' old
/// one, except that a hot-catch repeat now prints its census line too
/// (`CRATONVM_DBG_DEOPT` only).
pub fn claim_site_trap_policy(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    names: (&str, &str, &str),
    declaring_class_id: ClassId,
    policy: SiteTrapPolicy,
) -> bool {
    let apply = site_trap_policy_must_apply(verdicts, h, names, declaring_class_id);
    let (class_name, method_name, descriptor) = names;
    verdicts.note_ir_method_refused(class_name, method_name, descriptor, declaring_class_id);
    if !apply {
        if policy.charges_site_trap_repeat() {
            cratonvm_jit::ir::note_site_trap_repeat();
        }
        note_site_trap_repeat_census(
            verdicts,
            h,
            names,
            declaring_class_id,
            policy.census_name(),
        );
    }
    apply
}

/// t1 wave 7, lane NBB (LAMBDATRAP21-2 of
/// `docs/feature-designs/jit-ir-optimizations.md`): under
/// `CRATONVM_DBG_DEOPT`, one `[cratonvm-deopt] site-trap repeat #n of C.m`
/// line on the 1st, 16th, 256th, ... repeat of one method's already-claimed
/// decision, saying whether the IR ban holds -- the process-wide
/// `note_site_trap_repeat` counter names no method, and the jr21 8664 ns cliff
/// (thousands of repeats, ban not in effect) would have shown in one run.
/// Off the debug flag it costs one cached bool read.
pub fn note_site_trap_repeat_census(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    (class_name, method_name, descriptor): (&str, &str, &str),
    declaring_class_id: ClassId,
    policy: &str,
) {
    if !dbg_deopt_on() {
        return;
    }
    let Some(n) = verdicts.note_site_trap_repeat_of(h, declaring_class_id) else {
        return;
    };
    if cratonvm_jit::ir::site_trap_repeat_is_reported(n) {
        let ban =
            verdicts.ir_method_ban_holds(class_name, method_name, descriptor, declaring_class_id);
        eprintln!(
            "[cratonvm-deopt] site-trap repeat #{n} of {class_name}.{method_name}{descriptor} \
             (class id {}, policy {policy}): ban holds={ban}",
            declaring_class_id.as_u32(),
        );
    }
}



/// `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS` (JIT round 19 wave 14, TAIL12-1;
/// default on): the two sinks an optimizing body is entered from the interpreter
/// through (`deopt_resume`'s door sink, `execute`'s first-call sink) also ask
/// [`retier_hot_division_guard_trap`], which only the call-site service asked:
/// a body entered from an interpreted loop whose `idiv` guard keeps trapping
/// was never re-tiered. Read per charged trap (cold); not cached.
pub fn div_guard_retier_sinks_enabled() -> bool {
    true
}

/// TAIL12-1: the door / first-call sinks' entry to
/// [`retier_hot_division_guard_trap`], behind
/// `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS`. `opcode` is the TRAPPING scope's
/// (the frame's own for a single frame, the innermost scope's for a chain),
/// `bci` the charge site in the method the key names. The same counter, the
/// same once-per-method claim as the call-site service's, so a method that
/// traps through both counts once.
#[allow(clippy::too_many_arguments)]
pub fn retier_division_guard_trap_at_sink(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    reason: cratonvm_jit::deopt::DeoptReason,
    opcode: Option<u8>,
    bci: u32,
) {
    if !div_guard_retier_sinks_enabled() {
        return;
    }
    retier_hot_division_guard_trap(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        reason,
        opcode,
        bci,
    );
}

/// `CRATONVM_JIT_DIV_GUARD_RETIER` — default ON; `0`/`false`/`off`/`no` turns
/// [`retier_hot_division_guard_trap`] off. Read per call (j27 wave 7, lane
/// T1IC: was a process `OnceLock`): only a division-guard deopt trap asks.
pub fn division_guard_retier_enabled() -> bool {
    true
}

/// Catches one OPTIMIZING-tier method may run outside compiled code within
/// [`OPTIMIZING_CATCH_RETIER_WINDOW`] before it is re-tiered. The division
/// guard's threshold, for the same reason: a method that catches a handful of
/// exceptions keeps its optimizing body.
pub const OPTIMIZING_CATCH_RETIER_THRESHOLD: u32 = 16;

/// The window [`OPTIMIZING_CATCH_RETIER_THRESHOLD`] is counted over. A RATE,
/// unlike the division guard's lifetime count: the cost this policy removes is
/// paid per catch, so a method that catches once a minute is better served by
/// its optimizing body than by the single-pass one.
pub const OPTIMIZING_CATCH_RETIER_WINDOW: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Bound on the per-thread table below; a full table is cleared (counting
/// restarts -- slower, never wrong).
pub const OPTIMIZING_CATCH_SITE_CAP: usize = 1024;

/// `(vm identity, ir_method_memo_hash, declaring class id, the VM's redefine
/// epoch)`: [`DivisionGuardSiteKey`] without the bci, for the same reason
/// (interpreter round i1 wave 23): a key by names alone, or by the
/// `SharedVm`'s reissued address, lets another loader's copy, a redefined
/// version or the next VM count as this method.
pub type OptimizingCatchKey = (usize, u64, u32, u32);

thread_local! {
    /// Catches an optimizing-tier body had to leave compiled code for, per
    /// method, per thread: `(count in the current window, window start)`. Per
    /// thread for the reason `DIVISION_GUARD_TRAPS` is.
    pub static OPTIMIZING_CATCH_EXITS: std::cell::RefCell<
        rustc_hash::FxHashMap<OptimizingCatchKey, (u32, std::time::Instant)>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Count one catch at `key` at `now`; `true` EXACTLY ONCE per window, on the
/// catch that reaches `threshold` within `window` of the window's first catch.
/// Pure over `counts`.
pub fn note_optimizing_catch_exit<K: std::hash::Hash + Eq>(
    counts: &mut rustc_hash::FxHashMap<K, (u32, std::time::Instant)>,
    key: K,
    now: std::time::Instant,
    threshold: u32,
    window: std::time::Duration,
    cap: usize,
) -> bool {
    if counts.len() >= cap && !counts.contains_key(&key) {
        counts.clear();
    }
    let entry = counts.entry(key).or_insert((0, now));
    if now.saturating_duration_since(entry.1) > window {
        *entry = (0, now);
    }
    entry.0 = entry.0.saturating_add(1);
    entry.0 == threshold
}

/// Round 12 wave 3, lane exc: stop a hot `catch` from deoptimizing its
/// optimizing-tier method on every exception.
///
/// The optimizing tier compiles no handler blocks. A throw its body can catch
/// leaves through a reason-9 rethrow pad (`ir_lower::emit_rethrow_pads`), and
/// the handler -- and THE REST OF THAT INVOCATION -- runs in the interpreter
/// from a rebuilt frame (`route_jit_signal_exception` at an interpreter door,
/// `run_jit_callee_handler` at a compiled caller's). A method that catches in
/// a loop (`for (..) { try { kernel(); } catch (..) {} }`, the
/// `R12IroptBoundsEdges.round` and `R12IrbackNoTerminator.drive` shapes) pays
/// a frame rebuild per catch plus an interpreted remainder, forever:
/// `PendingException` is never charged, so nothing escalates. The single-pass
/// tier enters its own `catch` in compiled code (`jit_local_handler_lookup`).
///
/// So after [`OPTIMIZING_CATCH_RETIER_THRESHOLD`] such catches within
/// [`OPTIMIZING_CATCH_RETIER_WINDOW`], apply the policy
/// [`retier_hot_division_guard_trap`] applies to a hot zero divisor: ban the
/// IR tier for the method and deoptimize with `SpeculationFailed`, which
/// evicts and recompiles it single-pass. Claimed ONCE per method through the
/// same `claim_site_trap_decision` set, so repeats cannot escalate the method
/// to not-compilable.
///
/// Callers pass only catches of an optimizing body (`used_ir_backend`) whose
/// handler actually ran. Kill switch: `CRATONVM_JIT_IR_CATCH_RETIER=0`
/// (default on).
pub fn retier_hot_optimizing_catch(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
) {
    if !optimizing_catch_retier_enabled() {
        return;
    }
    let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
    let key: OptimizingCatchKey = (
        vm.vm_identity,
        h,
        declaring_class_id.as_u32(),
        vm.jit.jit_cache.redefine_epoch(),
    );
    let now = std::time::Instant::now();
    let reached = OPTIMIZING_CATCH_EXITS.with(|exits| match exits.try_borrow_mut() {
        Ok(mut counts) => note_optimizing_catch_exit(
            &mut counts,
            key,
            now,
            OPTIMIZING_CATCH_RETIER_THRESHOLD,
            OPTIMIZING_CATCH_RETIER_WINDOW,
            OPTIMIZING_CATCH_SITE_CAP,
        ),
        Err(_) => false,
    });
    if !reached {
        return;
    }
    // Already decided for this method (by this policy, the division guard's,
    // or a site trap): the IR tier is refused already. Not a site trap, so
    // `note_site_trap_repeat` is not charged either. This VM's verdicts, keyed
    // by the declaring class id (interpreter round i1 wave 23).
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    // JIT round 21 (lane revvm2, LAMBDATRAP21-1): unless the IR ban could not
    // be recorded (`claim_site_trap_policy`, which also records the ban).
    if !claim_site_trap_policy(
        verdicts,
        h,
        (class_name, method_name, descriptor),
        declaring_class_id,
        SiteTrapPolicy::HotCatch,
    ) {
        return;
    }
    // No bci names this decision; 0 keys the ledger row, as any bci would.
    let action = DeoptimizationController::deoptimize_or_by_name(
        vm,
        declaring_class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        0,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] hot-catch re-tier: {class_name}.{method_name}:{descriptor} \
             ({OPTIMIZING_CATCH_RETIER_THRESHOLD} catches outside compiled code within \
             {OPTIMIZING_CATCH_RETIER_WINDOW:?}) -> IR tier refused, action={action:?}"
        );
    }
}

/// `CRATONVM_JIT_IR_CATCH_RETIER` -- default ON; `0`/`false`/`off`/`no` turns
/// [`retier_hot_optimizing_catch`] off. Cached: read on a catch path.
pub fn optimizing_catch_retier_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// [`retier_hot_optimizing_catch`] for the compiled-caller door: a callee's
/// handler that `try_run_callee_handler` just ran in the interpreter. That door
/// holds no `CompiledMethod`, so the tier is read off the body the cache
/// publishes for the callee; a miss (another key, nothing published) or a
/// single-pass body is not counted.
pub fn note_optimizing_callee_catch(
    vm: &SharedVm,
    callee: &crate::classloading::resolution::CachedBytecodeMethod,
) {
    if !optimizing_catch_retier_enabled() {
        return;
    }
    let optimizing = vm
        .jit
        .jit_cache
        .read()
        .get(
            &callee.class_name,
            &callee.method_name,
            &callee.method_descriptor,
            callee.declaring_class_id,
        )
        .is_some_and(|body| body.used_ir_backend);
    if optimizing {
        retier_hot_optimizing_catch(
            vm,
            &callee.class_name,
            &callee.method_name,
            &callee.method_descriptor,
            callee.declaring_class_id,
        );
    }
}


/// Does the receiver at this call site resolve `info`'s SAM to the method the
/// stashed frame names?
///
/// This is the identity `try_resume_trapped_callee` could not supply from
/// `info` alone, and the one `try_lambda_site_direct_call` already spends on
/// the other door (`site.cached_impl()`). It is read from the metafactory's own
/// record — `LambdaCallSite::impl_handle`, the handle the `invokedynamic`
/// bootstrap bound — rather than from `lambda_jit_site`, whose eligibility
/// gates (static impl, identity coercion, no exception table, the
/// `CRATONVM_JIT_LAMBDA_SITE` switch) decide whether a FAST PATH may serve the
/// call and have nothing to say about which method a trapped frame belongs to.
/// Measured on `DeoptLambdaRerunCount`: that door's `calls` counter is 0 for
/// the shape this arm has to answer for, so keying on it would have been a fix
/// that never fires.
///
/// Every clause is an equality, and all four must hold. `false` for a receiver
/// that is not a lambda proxy, which is the overwhelmingly common case and
/// costs one `FxHashMap` probe under a read lock.
pub fn lambda_site_resolves_to(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    // `ClassId::new(0)` is the "no receiver" value the dispatch helpers pass for
    // a statically bound site. A lambda SAM call always has one.
    if receiver_class_id.as_u32() == 0 {
        return false;
    }
    let Some(call_site) = vm.classes.lambda_call_site_for(receiver_class_id) else {
        return false;
    };
    // The proxy must answer THIS call — name and descriptor both, the same pair
    // `LambdaJitSite::serves` asks. A default method on the functional
    // interface reaches the same receiver and is not the SAM.
    if &*call_site.sam_method_name != info.method_name
        || !descriptors_match_modulo_return(&call_site.sam_descriptor, info.descriptor)
    {
        return false;
    }
    let h = &call_site.impl_handle;
    (&*h.class_name == key_class && &*h.member_name == key_method && &*h.descriptor == key_desc)
        // JIT round 20 wave 12 (lane qLam, DISP20-3): an instance-method impl
        // entered through a receiver thunk is the body the RECEIVER's class
        // resolves to, which the handle does not name (`Derived.m` for a
        // `Base::m` handle). Same member and descriptor, a virtual / interface
        // handle, the switch on (no receiver thunk exists otherwise), and the
        // trapping body is a virtual target of the handle (its class is the
        // stash's and a subtype of the handle's) or a site of this proxy on
        // this thread binds exactly that method.
        || (matches!(
            h.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        ) && &*h.member_name == key_method
            && &*h.descriptor == key_desc
            && crate::runtime::interpreter::lambda_instance_thunk_enabled()
            && (lambda_stash_body_is_virtual_target_of(vm, &h.class_name, key_class)
                || crate::runtime::interpreter::lambda_instance_site_binds(
                    vm,
                    receiver_class_id,
                    key_class,
                    key_method,
                    key_desc,
                )))
}

/// JIT round 20 wave 12, lane qLam (DISP20-3): is the body that left the
/// pending stash one a virtual dispatch of `handle_class`'s method can select
/// -- published under a class (the cache key: the receiver's class) that is
/// assignable, by name, to both `key_class` (the stash's declaring class) and
/// `handle_class`? `false` when the stash names no
/// published body. Asked only by [`lambda_site_resolves_to`] on a refusal of
/// the handle's own identity.
pub fn lambda_stash_body_is_virtual_target_of(
    vm: &SharedVm,
    handle_class: &str,
    key_class: &str,
) -> bool {
    let Some(body_class) = cratonvm_jit::deopt::peek_last_deopt_trapping_body()
        .and_then(|body| body.published_class_id())
        .filter(|&id| id != 0)
        .map(ClassId::new)
    else {
        return false;
    };
    // JIT r20 lane rev12 (correctness, unconditional): the published id is
    // the CACHE KEY class -- the receiver's class for an inherited body
    // (`Derived` for a `Base.m` body), not the declaring class the stash key
    // names -- and an interface handle (`I::f`, `List::size`) is reached
    // through the interface DAG, which `is_subclass_of_by_name` (supers only)
    // never walks. Either miss refused the stash on every thread but the one
    // whose site cache bound the method, although the receiver thunk lives in
    // shared code; the refused stash was then orphaned and the impl re-run
    // from entry. The body's class must reach both the stash's declaring
    // class and the handle's class, supers and interfaces alike.
    let cm = vm.classes.class_manager.read();
    cm.is_assignable_to_name(body_class, key_class)
        && cm.is_assignable_to_name(body_class, handle_class)
}

/// What actually trapped in a compiled callee, read from the callee's own
/// artifact rather than assumed — N3(b).
///
/// # Why this is not just `UnreachedCode`
///
/// `try_resume_trapped_callee`'s de-speculation call passed a hard-coded
/// `UnreachedCode`, and `UnreachedCode` is the single reason whose
/// `DeoptimizationLog::recommend_action` is `MakeNotCompilable` on the FIRST
/// occurrence. So the *success* path — the precise, no-side-effect-replayed
/// resume of a trapped callee, the outcome the whole stash machinery exists to
/// produce — permanently retired the callee after one trap of any kind: a
/// `ReceiverTypeChanged` guard that a recompile would re-profile correctly, a
/// `BoundsCheck` speculation, a `NullCheck`, all filed as "this code is
/// unreachable".
///
/// The evidence is in the artifact: the stub baked a `DeoptimizationPoint`, and
/// `osr_exit::deopt_reason_at_bci` reads it — skipping the `rethrow_exception`
/// points (an ordinary stash from `take_last_deopt` cannot have come from one)
/// and refusing rather than guessing when what remains disagrees. That is the
/// same disambiguation `trap_reason_for_frame` performs for the stash sinks in
/// `deopt_resume.rs`; this site differs only in that it has the callee's names
/// and bci rather than a `ReconstructedFrame`, so it must find the artifact
/// itself.
///
/// `UncommonTrap` is the fallback for every way the lookup can come up empty —
/// no artifact in the cache (it may have been evicted between the trap and this
/// call), no point at the bci, or two points that disagree. It is CHARGED, so
/// the trap rate stays visible and the method still backs off, and it escalates
/// on the ordinary count-based schedule rather than one-shot, so an attribution
/// nobody could make can never blacklist a method by itself.
///
/// Reserve `UnreachedCode` for genuinely unreachable code.
pub fn trapped_callee_deopt_reason(
    vm: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    bci: u32,
    frame: Option<(&[u8], &cratonvm_jit::deopt::ReconstructedFrame)>,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    stashed_point: usize,
    sibling_body: Option<cratonvm_jit::RetainedCode>,
) -> cratonvm_jit::deopt::DeoptReason {
    // `class_id` is the resolved declaring class the caller already holds —
    // the JIT cache's own key. The by-name `get_loaded_class_id` this used
    // answers `None` when two loaders define the name, and the lookup then
    // missed the artifact and charged `UncommonTrap` for a nameable guard.
    let published = {
        let cache = vm.jit.jit_cache.read();
        cache.get(class_name, method_name, descriptor, class_id)
    };
    // JIT round 19 wave 11, lane tail11 (DEOPT10-1 of
    // `jit-r19-deopt10-proposals-RETIRED-20261009.md`, opt-in
    // `CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY`): ask the body the stash
    // NAMED (`taken_trapping_body`) before the published one. A charged trap
    // is never a superseded body's (the caller's check), so the two agree
    // whenever the slot holds that body. They differ for an EMPTY slot (the
    // body withdrawn by a redefinition or an interpreter-only request while
    // its activation ran), where the published lookup answers `None` and the
    // trap was charged `UncommonTrap` -- the wrong speculation id for a
    // nameable guard; and for a body published by another thread BETWEEN the
    // caller's superseded check and this read, whose points at `bci` describe
    // another compile. Off: the published body only, as before.
    let named_on = charge_reason_from_named_body_enabled();
    let named = (stashed_point != 0
        && (named_on || published.is_none())
        && cratonvm_jit::deopt::stash_names_trapping_body_enabled())
    .then(|| stale_trap_not_entrant::taken_trapping_body_retained(stashed_point))
    .flatten();
    // j27 wave 7 (lane T1LAM3): every handle below is a `RetainedCode`, so
    // whichever is the last owner releases through the retirement queue.
    let published = published.map(cratonvm_jit::RetainedCode::new);
    if published.is_none() && dbg_deopt_on() {
        // The census that decides DEOPT10-1: a charge with an empty slot,
        // and whether the stash named the body that trapped.
        eprintln!(
            "[cratonvm-deopt] callee trap charged with an empty slot: \
             {class_name}.{method_name}:{descriptor} bci={bci} named-body={} \
             used={} (CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY)",
            named.is_some(),
            if named_on { "named" } else { "published" },
        );
    }
    // A trap of a body published under ANOTHER key of the same method (the
    // receiver class's, for an inherited method; `sibling_key` at the charge
    // site) is read from THAT body. The slot the declaring key publishes holds
    // a different compile, whose points at `bci` need not include the guard
    // that fired: `CharsetDecoder.decode(BB,CB,Z)`'s loop-header poll exit
    // (`OsrExit`, bci 54) was then the only point there, the trap was charged
    // as an `OsrExit` and, two charges later, `MakeNotCompilable` -- the method
    // interpreted for the rest of the run -- while the `UncommonTrap` that
    // actually fired was never de-specialized and every recompile carried it
    // again.
    let compiled = if sibling_body.is_some() {
        sibling_body
    } else if named_on {
        named.or(published)
    } else {
        published
    };
    match (compiled.as_deref(), frame) {
        // With the reconstructed frame in hand, the rule every other sink
        // applies (`trap_reason_for_frame`: rethrow and OSR-exit points
        // skipped, the array NullCheck/BoundsCheck pair told apart by the
        // array reference on the frame's operand stack). Before round 11
        // wave 3 this sink alone read the bare bci, so one trap was charged
        // under a different reason depending on which sink caught it.
        (Some(compiled), Some((code, rframe))) => {
            crate::runtime::interpreter::trap_reason_for_frame_with_cause(
                compiled, code, rframe, cause,
            )
        }
        (compiled, _) => {
            deopt_reason_from_artifact(compiled.map(|c| &c.deopt_points[..]), bci, cause)
        }
    }
}

/// `CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY` (JIT round 19 wave 11, lane
/// tail11, DEOPT10-1; opt-in): [`trapped_callee_deopt_reason`] reads an
/// empty-slot trap's reason from the body the stash named. Read per charge
/// (cold); not cached.
pub fn charge_reason_from_named_body_enabled() -> bool {
    false
}

/// The decision half of [`trapped_callee_deopt_reason`], with the artifact
/// lookup already done.
///
/// Split out so the rule is testable without a live `SharedVm`: every arm here
/// is a policy choice about what to charge, and every one of them used to be a
/// hard-coded `UnreachedCode`. `None` for the slice means the artifact was not
/// in the cache — it can be evicted between the trap and this call, and that is
/// not evidence about the method either.
///
/// Since interpreter round i1 wave 49 (lane L2, behind
/// `cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`) the stash's own
/// `cause` is believed first, under `trap_cause_at_frame`'s rule (a
/// non-rethrow point of this artifact at `bci` with that reason and, for a
/// non-wildcard id, that id): the frame-less arm (an inlined chain) was the
/// one production sink that still dropped it, and an `OsrExit` map may now
/// share its bci with a guard, where only the cause says which stub fired.
pub fn deopt_reason_from_artifact(
    points: Option<&[cratonvm_jit::deopt::DeoptimizationPoint]>,
    bci: u32,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    if let (true, Some(points), Some(c)) = (
        cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS,
        points,
        cause,
    ) {
        let named = points.iter().any(|p| {
            p.bci == bci
                && p.reason == c.reason
                && !p.semantics.rethrow_exception
                && (c.speculation_id == cratonvm_jit::deopt::SPECULATION_ID_ANY
                    || p.speculation_id == c.speculation_id)
        });
        if named {
            return c.reason;
        }
    }
    match points {
        Some(points) => match cratonvm_jit::osr_exit::deopt_reason_at_bci(points, bci) {
            cratonvm_jit::osr_exit::ReasonAtBci::Unique(r) => r,
            cratonvm_jit::osr_exit::ReasonAtBci::Ambiguous
            | cratonvm_jit::osr_exit::ReasonAtBci::None => {
                cratonvm_jit::deopt::DeoptReason::UncommonTrap
            }
        },
        None => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
    }
}

/// [`despeculate_trapped_method_of_frame`] with the per-bci de-spec's grain
/// given by the caller: `Some(c)` counts at `(c.reason, bci)` and withdraws
/// `c.speculation_id` (the doors' rule for an attribution they named), `None`
/// keeps the inner function's own rule. An IR site trap always records the
/// wildcard. Round nio21 wave 7, lane L2 (folded onto dev's frame-aware form
/// at the nio21 merge; placed after [`despeculate_trapped_method_inner`] so
/// the `r19w3_eager3_tests` source window still reaches the inner body).
#[allow(clippy::too_many_arguments)]
pub fn despeculate_trapped_method_of_frame_despec_as(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    despec_as: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    let is_chain = !rframe.caller_frames.is_empty();
    let chain = (is_chain && cratonvm_jit::ir::ir_splice_expected_throw_stash_sinks_enabled())
        .then_some(rframe);
    // JIT round 19 wave 12, lane tail12: the switch-off chain records the
    // wildcard at the call site (`helper_chain_site_wildcard_enabled`).
    let chain_site_wildcard = is_chain && chain.is_none() && helper_chain_site_wildcard_enabled();
    despeculate_trapped_method_inner(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        chain,
        chain_site_wildcard,
        despec_as,
    );
}



/// The doors' chain attribution (`deopt_resume::stash_charge_site`, the
/// chain arm of `stash_trap_attribution`) for the call-site service: for an
/// inlined chain stash, the stash's own reason (else `UncommonTrap`) under the
/// wildcard speculation id, to be charged and de-spec'd at the call site.
/// `None` for a single frame, the superseded-guard sentinel, or under
/// `CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE=0`. (The W3L4-4 nested-trap named
/// id needs the trapping artifact's spliced sources, which this sink does not
/// hold; the wildcard is the coarser answer the doors gave before W3L4-4.)
pub fn chain_trap_charge(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    if rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
    {
        return None;
    }
    Some(cratonvm_jit::deopt::DeoptCause {
        reason: cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason),
        // JIT round 23 wave 4 (lane L1): a spliced loop's entry predicate is
        // named by its chain id, which the planter asks at the call site;
        // every other chain trap keeps the wildcard.
        // JIT round 25 (item 4 residual 1): so is a spliced field site's
        // compactness guard (`chain_named_charge_id`).
        speculation_id: crate::runtime::interpreter::chain_named_charge_id(rframe, cause)
            .unwrap_or(cratonvm_jit::deopt::SPECULATION_ID_ANY),
    })
}

/// JIT round 21 wave 1 (lane loops, LOOPS21-2): under
/// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, the per-bci grain of a
/// single-frame trap whose stashed cause is a loop-entry predicate guard's
/// (`UncommonTrap` under a planter's id,
/// `cratonvm_jit::deopt::is_predicate_speculation_id` at the trap's bci):
/// counted at `(UncommonTrap, bci)`, withdrawn under that id, so the next
/// compile drops that planter's predicate alone. `None` (the sink's own rule:
/// the wildcard for an unnamed `UncommonTrap`) for a chain, the
/// superseded-guard sentinel, any other cause or reason, or the switch off.
pub fn predicate_trap_charge(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    bci: u32,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    trap_reason: cratonvm_jit::deopt::DeoptReason,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    use cratonvm_jit::deopt::DeoptReason;
    if !rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || rframe.bci != bci
        || trap_reason != DeoptReason::UncommonTrap
        || !cratonvm_jit::deopt::predicate_own_speculation_enabled()
    {
        return None;
    }
    let c = cause?;
    (c.reason == DeoptReason::UncommonTrap
        && cratonvm_jit::deopt::is_predicate_speculation_id(bci, c.speculation_id))
    .then_some(c)
}

/// JIT round 23 wave 3 review (`CRATONVM_JIT_OSR_PREDICATE_EXIT_CHARGE`;
/// `docs/feature-designs/deopt-osr.md`):
/// charge an optimizing OSR body's loop-entry predicate exit exactly as the
/// trapped-frame sink charges a method-entry body's predicate trap. Counted at
/// `(UncommonTrap, bci)`. It is withdrawn under the planter's own id when
/// [`predicate_trap_charge`] names it (`CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`),
/// and otherwise under the wildcard at the header, which
/// `cratonvm_jit::deopt::predicate_withdrawn` and the OSR arm checks both
/// read. The caller (`jit_bridge::charge_osr_guard_exit`) has already decided
/// that the exit is such a predicate (`jit_bridge::osr_predicate_exit_charge`)
/// and that the frame is flat.
pub fn despeculate_osr_predicate_exit(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    let reason = cratonvm_jit::deopt::DeoptReason::UncommonTrap;
    let despec_as = predicate_trap_charge(rframe, rframe.bci, cause, reason);
    despeculate_trapped_method_of_frame_despec_as(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        reason,
        rframe.bci,
        rframe,
        despec_as,
    );
}


/// The reason a trapped INLINED CHAIN is charged under at the call-site
/// service, under `CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS` (JIT round 19
/// wave 10, lane deopt10; opt-in): the stash's own cause, else
/// `UncommonTrap` -- `deopt_resume::stash_charge_site`'s chain arm, so the
/// door sinks and this one charge one chain trap alike. The trap fired at a
/// bci of a spliced callee; the outer method's points at the outermost
/// scope's bci describe the call site, not the guard that failed.
pub fn chain_trap_charge_reason(
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason)
}

/// Is this stashed machine point one where the collector may have transferred
/// the call site's service-copy roots to the callee? Such a frame must never
/// take the declined-callee entry-replay path.
pub fn service_copy_resume_only_at(point_addr: usize, bci: u32) -> bool {
    let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(point_addr) else {
        return false;
    };
    // SAFETY: the code-range registry retains CompiledMethod metadata while
    // frames from that body can remain on a live thread's stack.
    let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
    !cm.self_locks_monitor
        && cm.method_declares_no_handlers
        && cm.service_arg_resume_only_sp_ids.iter().any(|id| {
            cm.safepoint_bci_table
                .iter()
                .any(|&(sp_id, sp_bci)| sp_id == *id && sp_bci == bci)
        })
}

/// A pure deopt in the callee this call site invoked, still stashed because
/// [`try_resume_trapped_callee`] declined to resume it (a redefined callee, a
/// class the site's loader cannot resolve, a synchronized callee, a frame that
/// cannot be materialised): re-run that callee from entry in the interpreter
/// and hand its result, value or pending throwable, to the compiled caller.
///
/// Round 11 wave 12 (lane tier's patch
/// `r11w12-tier-helpers-declined-callee-rerun-patch-FIXED-20260924.md`, applied by
/// lane hashmap). Left stashed, the frame reached a sink that could not resume
/// it either: an OSR'd caller replayed its loop from the entry pc (`try_osr`'s
/// identity-mismatch arm), and a method-entry caller re-ran from bci 0. Both
/// call the callee again, so re-running only the callee duplicates strictly
/// less. The stash is charged as those sinks charged it.
///
/// `None`, touching nothing, when the stash does not name this site's callee.
///
/// The arguments are read through `pins` AFTER the de-speculation (as the
/// KCFULL-13 re-run reads them), so a collection in between cannot hand the
/// interpreter a stale reference word where the caller pinned one.
///
/// # Safety
/// As [`try_resume_trapped_callee`].
pub unsafe fn rerun_declined_callee_from_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> Option<i64> {
    // The identity question `try_resume_trapped_callee` asks, on the same
    // (outermost) scope.
    let (key, _) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity()?;
    let (rest, key_desc) = key.rsplit_once(':')?;
    let (key_class, key_method) = rest.rsplit_once('.')?;
    let names_this_callee = (key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !names_this_callee
        || !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc)
    {
        return None;
    }
    let (rframe, cause, point_addr) = cratonvm_jit::deopt::take_last_deopt_with_point()?;
    // The callee's frame is dropped and the callee re-runs from entry, so what
    // its compiled code locked is released first; the re-run takes its own
    // (interpreter round i1 wave 23, lane L2). Pinned before the charge below.
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    if service_copy_resume_only_at(point_addr, rframe.bci) {
        // The GC has already been allowed to drop the caller's copies for this
        // point. Do not turn a precise-resume refusal into an entry replay that
        // reads those dead slots.
        crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
            vm, &rframe, cause, point_addr,
        );
        compiled_locks.release_for_a_rerun(vm, thread);
        let _ = take_jit_deopt_pending();
        let callee = callee_rerun_code(vm, info, receiver_class_id);
        drop(rframe);
        return Some(refuse_callee_rerun(
            vm,
            thread,
            info,
            callee.as_ref(),
            crate::runtime::interpreter::DoorRerunCause::CalleeDeclined,
            "a service-copy resume-only frame was declined",
        ));
    }
    // Round 12 wave 5 (lane replay3): counted in the door census with the
    // replay rule's verdict for the callee's own bytecode, so the double
    // commits this service still makes are sized (`callee-declined`).
    //
    // Round 13 wave 8 (lane replay4): a stash class the site's loader cannot
    // resolve (one of the declines that brought the stash here) is answered
    // from the method the re-run dispatches to, by the whole-body rule, where
    // it used to read unsound unchecked; and an unsound re-run is refused.
    //
    // Round 13 wave 9 (lane replay5): judged BEFORE the charge below. The
    // verdict reads the published body (its `spliced_bodies_side_effect_free`
    // and whether it owns the stashed point), and the charge evicts that body
    // by name (`DeoptimizationController::deoptimize_in`), so judged after it
    // every storing callee read UNSOUND whatever its trap bci (the `None`
    // artifact arm assumes impure splices): a spurious `InternalError` where
    // the prefix rule proves the re-run exact. Kill switch
    // `CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST=0`.
    let verdict = || {
        callee_rerun_replay_is_exact(
            vm,
            info.declaring_class_id,
            key_class,
            key_method,
            key_desc,
            Some((&rframe, point_addr)),
        )
        .unwrap_or_else(|| {
            callee_rerun_code(vm, info, receiver_class_id).is_some_and(|(_, _, code)| {
                crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                    &code,
                    false,
                    u32::MAX,
                )
            })
        })
    };
    let judged_first = true
    .then(&verdict);
    // Not charged when the callee left for the interpreter-only mode (wave 15).
    crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
        vm, &rframe, cause, point_addr,
    );
    compiled_locks.release_for_a_rerun(vm, thread);
    // The flag a frame-stashing trap raises beside its stash is spent with it.
    let _ = take_jit_deopt_pending();
    let exact = judged_first.unwrap_or_else(verdict);
    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeDeclined;
    crate::runtime::interpreter::note_door_rerun(cause, exact);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] declined callee trap re-run at the call site: {}.{}{} \
             (stash {} bci={}) replay={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            rframe.method_key,
            rframe.bci,
            if exact { "exact" } else { "UNSOUND" },
        );
    }
    drop(rframe);
    // A redefined callee keeps its re-run (the owner decision for a hot swap;
    // `try_resume_trapped_callee` declines every stash of a body compiled
    // before its class's last redefinition, and this is where those land).
    // By name, or any redefinition at all when the name is another loader's
    // too: the conservative side keeps the re-run.
    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
        named_class_was_redefined(vm, key_class)
            || (crate::classloading::any_class_redefined()
                && vm
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(key_class)
                    .is_none())
    }) {
        let callee = callee_rerun_code(vm, info, receiver_class_id);
        return Some(refuse_callee_rerun(
            vm,
            thread,
            info,
            callee.as_ref(),
            cause,
            "a declined stash",
        ));
    }
    let bail_args = decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
    Some(bail_to_interpreter(vm, thread, info, &bail_args))
}

/// gcd d9/d (2026-09-28): the callee-owned-arguments twin of
/// [`rerun_declined_callee_from_entry`], for a call site that kept no copy of
/// its arguments (`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`).
///
/// Everything that function does before its re-run -- the same identity
/// question on the outermost scope, the take, the release of what the
/// callee's compiled code locked, the charge, the spent deopt flag -- and no
/// re-run: there are no arguments to run it with. The caller then leaves as
/// a frameless deopt ([`handle_compiled_callee_deopt_sentinel`] raises the
/// flag again) and is re-run by its own caller, whose arguments that caller
/// still holds.
///
/// `None`, touching nothing, when there is no stash or it does not name this
/// site's callee (a foreign stash keeps propagating, as before). Otherwise
/// the callee replay's verdict (gce e1/f: the hand-off's record,
/// [`FramelessTrapSite::Handoff`]).
pub fn drop_declined_callee_stash(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<bool> {
    let (key, _) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity()?;
    let (rest, key_desc) = key.rsplit_once(':')?;
    let (key_class, key_method) = rest.rsplit_once('.')?;
    let names_this_callee = (key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !names_this_callee
        || !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc)
    {
        return None;
    }
    let (rframe, cause, point_addr) = cratonvm_jit::deopt::take_last_deopt_with_point()?;
    // gcd d10/f: the callee is re-run from entry all the same -- inside the
    // caller's re-run, whose own prefix the site's admission proved commits
    // nothing (`x64/op_invoke.rs`, `owned_args_handoff_replays_exactly`) --
    // so the callee's replay is counted exactly as the copying site's
    // `rerun_declined_callee_from_entry` counts it (`callee-declined`): judged
    // BEFORE the charge below, which evicts the body the stash rule reads
    // (round 13's `CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST` ordering), and
    // with the same whole-body fallback when the method cannot be found.
    let exact = callee_rerun_replay_is_exact(
        vm,
        info.declaring_class_id,
        key_class,
        key_method,
        key_desc,
        Some((&rframe, point_addr)),
    )
    .unwrap_or_else(|| {
        // SAFETY: `callee_rerun_code`'s contract is `resolve_callee_uncached`'s
        // (`vm` live, `info` a live `JitInvokeInfo`), which this function's
        // only caller, `handle_compiled_callee_deopt_sentinel`, holds.
        unsafe { callee_rerun_code(vm, info, receiver_class_id) }.is_some_and(|(_, _, code)| {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                &code,
                false,
                u32::MAX,
            )
        })
    });
    // As the re-run: the frame is dropped, so what its compiled code locked is
    // released, through pins taken before the charge (which can collect).
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
        vm, &rframe, cause, point_addr,
    );
    compiled_locks.release_for_a_rerun(vm, thread);
    let _ = take_jit_deopt_pending();
    crate::runtime::interpreter::note_door_rerun(
        crate::runtime::interpreter::DoorRerunCause::CalleeDeclined,
        exact,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] declined callee trap dropped at an owned-arguments call site: \
             {}.{}{} (stash {} bci={}) callee replay={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            rframe.method_key,
            rframe.bci,
            if exact { "exact" } else { "UNSOUND" },
        );
    }
    Some(exact)
}

/// gcd d9/d (2026-09-28): name, under `CRATONVM_DBG_DEOPT`, a callee trap an
/// owned-arguments call site hands to the caller's caller instead of
/// re-running the callee (`why`: which arm could not run). Rare by the
/// site's own admission rule (`sp_direct_call_args_owned_by_callee` and
/// `cratonvm_jit::callee_body_owns_its_arguments`); a workload where these
/// lines are frequent is one where `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` should
/// be compared.
pub fn note_callee_owned_args_trap_propagated(info: &JitInvokeInfo, why: &str) {
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] owned-arguments call site: {why} of {}.{}{} leaves as the \
             caller's frameless deopt (no argument copy to re-run it with)",
            info.class_name, info.method_name, info.descriptor,
        );
    }
}

/// The replay rule's verdict for a call-site service about to re-run a callee
/// from entry, for the door census (round 12 wave 5, lane replay3;
/// `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns` cause 4).
///
/// `class_name` is read as the method's DECLARING class (a stash key names
/// it), resolved from `from_class_id`'s loader (`0`: the bootstrap loader) as
/// `try_resume_trapped_callee` resolves it; only that class's own methods are
/// searched. `stash` is the callee's frame with its point address; `None`
/// asks the whole-body rule. A method this cannot find is answered `None`
/// (round 13 wave 8: the caller decides; a census must not call exact a
/// re-run it could not check).
pub fn callee_rerun_replay_is_exact(
    vm: &SharedVm,
    from_class_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    stash: Option<(&cratonvm_jit::deopt::ReconstructedFrame, usize)>,
) -> Option<bool> {
    let found = {
        let cm = vm.classes.class_manager.read();
        let class_id = if from_class_id == 0 {
            cm.find_bootstrap_class_by_name(class_name)
        } else {
            cm.find_cp_class_for_class(class_name, ClassId::new(from_class_id))
        };
        class_id.and_then(|class_id| {
            let class = cm.class_store().get(class_id)?;
            let method = class
                .methods
                .iter()
                .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)?;
            let code_attr = method.code()?;
            Some((
                class_id,
                crate::runtime::frame::padded_bytecode_for_method(
                    class_id,
                    method_name,
                    descriptor,
                    &code_attr.code,
                    &code_attr.exception_table,
                ),
            ))
        })
    };
    let (class_id, code) = found?;
    let artifact = vm
        .jit
        .jit_cache
        .read()
        .get(class_name, method_name, descriptor, class_id);
    // A chain's `bci` is its innermost scope's, not this method's: only the
    // whole-body rule can answer for the outermost method.
    let stash = stash.filter(|(rframe, _)| rframe.caller_frames.is_empty());
    Some(match (stash, artifact.as_deref()) {
        (Some((rframe, point)), Some(ran)) => {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_for_stash(
                &code, ran, rframe, point,
            )
        }
        (Some((rframe, _)), None) => {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                &code, false, rframe.bci,
            )
        }
        // Round 13 wave 1 (lane replay): a body whose frameless traps the
        // single-pass finalizer proved exact re-runs exactly, as the doors'
        // verdict reads it (`door_rerun_verdict`).
        (None, ran) => {
            ran.is_some_and(|r| r.frameless_traps_replay_exact)
                || crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                    &code,
                    ran.is_some_and(|r| r.spliced_bodies_side_effect_free),
                    u32::MAX,
                )
        }
    })
}

/// Was the trap stashed right now taken in the body currently published for
/// `key_class.key_method key_desc`, compiled after that class's last
/// redefinition (interpreter round i1 wave 23, lane L6)? Such a body ran the
/// class's CURRENT bytecode, which is what [`try_resume_trapped_callee`]
/// rebuilds the frame from, so the frame resumes exactly -- as the
/// interpreter door's sink already resumes a fresh body of a redefined class
/// (`deopt_resume`'s `compilation_epoch` test). Refusing it re-ran the callee
/// from entry (`rerun_declined_callee_from_entry`) and replayed every effect
/// it had committed before the trap: for EVERY guard trap in a compiled
/// callee of a class an agent had retransformed once, however long ago. The
/// not-entrant stub's forwarding (`NotEntrantRecord::forward_to`) sends a
/// patched body's callers straight into exactly such a body.
///
/// Identity by the stashed point's address: it must lie in that body's
/// `deopt_points` or be one of its boxed points. A superseded body, one
/// published under another key (an inherited method under a receiver's key),
/// or a class the call site's loader does not resolve answers `false`, the
/// conservative refusal -- except (wave 61, lane L2) a body the stash itself
/// names, which answers by its own compile epoch whether or not it is still
/// published.
pub fn stash_is_from_a_current_body(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    let point = cratonvm_jit::deopt::peek_last_deopt_point();
    if point == 0 {
        return false;
    }
    let class_id = {
        let cm = vm.classes.class_manager.read();
        if info.declaring_class_id == 0 {
            cm.find_bootstrap_class_by_name(key_class)
        } else {
            cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id))
        }
    };
    let Some(class_id) = class_id else {
        return false;
    };
    let by_name = vm
        .jit
        .jit_cache
        .get(key_class, key_method, key_desc, class_id)
        .is_some_and(|body| {
            body.owns_deopt_point(point)
                && vm
                    .jit
                    .jit_cache
                    .compiled_since_redefinition_of(class_id, body.install_epoch)
        });
    // Interpreter round i2 wave 61, lane L2: the superseded test asks the
    // body the trampoline named first (`named_trapping_body_is_current`); a
    // body superseded by a tier-up after a compile that began past the
    // redefinition ran the current bytecode too.
    named_trapping_body_is_current(vm, class_id, point, by_name, "current-body").unwrap_or(by_name)
}

/// gch h2/fj (`gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection-20260928`,
/// step 1): push every object word of a frame just TAKEN out of the deopt
/// stash onto `pins` (a thread's `native_pin_roots`), in
/// [`cratonvm_jit::deopt::ReconstructedFrame::for_each_object_address`]
/// order -- locals, stack, monitors, scalar-replaced objects' fields, caller
/// scopes. Out of the stash nothing else roots them, and the rebuild that
/// follows can collect. Answers how many were pushed.
pub fn pin_taken_stash_frame_words(
    pins: &mut Vec<ObjectRef>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> usize {
    let base = pins.len();
    rframe.for_each_object_address(&mut |addr: u64| {
        // SAFETY: a non-null heap reference (the visitor never offers `0`)
        // the stash rooted until the take; it is pinned before anything can
        // collect.
        pins.push(unsafe { ObjectRef::from_raw(addr as usize as *mut u8) });
    });
    pins.len() - base
}

/// Step 2 of the same proposal: write the pins [`pin_taken_stash_frame_words`]
/// pushed at `pins[base..base + count]` (rewritten in place by any moving
/// collection since) back into `rframe`'s object words, in the same order, so
/// the frame names current addresses and may go back into the stash. Answers
/// `false`, leaving `rframe` untouched, when the pin window or the frame's
/// word count no longer matches (the caller then drops the frame, as before).
pub fn refresh_taken_stash_frame_words(
    pins: &[ObjectRef],
    base: usize,
    count: usize,
    rframe: &mut cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    let Some(window) = base.checked_add(count).and_then(|end| pins.get(base..end)) else {
        return false;
    };
    let mut words = 0usize;
    rframe.for_each_object_address(&mut |_addr: u64| words += 1);
    if words != count {
        return false;
    }
    let mut k = 0usize;
    rframe.for_each_object_address_mut(&mut |addr: u64| {
        let now = window.get(k).map(|pin| pin.as_ptr() as usize as u64);
        k += 1;
        now.filter(|&now| now != addr)
    });
    true
}




/// HANDLER11-4 (JIT round 19 wave 12, lane handler12;
/// `r19-callsite-service-claims-a-stash-by-name-and-parameters-only-FIXED-20261012.md`):
/// may the call-site services ([`try_resume_trapped_callee`],
/// [`rerun_declined_callee_from_entry`], [`drop_declined_callee_stash`]) claim
/// the innermost stash entry, which already names this site's callee by method
/// name and parameter descriptor (`key_method` is the stash's outermost
/// method)? Those two terms alone let an ORPHAN -- a frame some earlier
/// consumer left -- be resumed as this call's callee whenever the names agree:
/// another class's namesake, whose result word need not even be of the
/// caller's kind (the `0x7`-reference reading of the R16LocaleWide stress).
///
/// Under the switch two more terms, both fail-open where nothing is known:
///
/// * the entry was not stamped as left by a consumer that already completed
///   its call (`cratonvm_jit::deopt::peek_last_deopt_left_by`, HANDLER11-3):
///   such an entry is nobody's live trap;
/// * the trapping body the trampoline named
///   (`cratonvm_jit::deopt::peek_last_deopt_trapping_body`) was published under
///   a class this site can reach: the receiver's class or a supertype of it
///   for a virtual or interface site with a known receiver, the class the
///   instruction names or a supertype of it for a static or special one. A
///   body without that record, a site whose class does not resolve, or a
///   lambda site (the stash names the impl, not `info`'s method) passes.
///
/// JIT round 20 (lane stash): a receiver whose class the class store does not
/// hold passes too (fail-open: no supertype question can be answered for it).
/// That is every lambda proxy (a VM-internal id, `LAMBDA_PROXY_ID_BASE` and
/// up): a proxy whose metafactory bound an impl that carries the SAM's own
/// name (`task::run` as a `Runnable`, `Integer::compare` as a `Comparator`)
/// names this site's callee by the name arm, and a default method of the
/// functional interface (`andThen`, `reversed`) by name too; its trapping
/// body's class is the impl's or the interface's, which `is_subclass_of`
/// cannot relate to an id it does not hold. Refused, such an entry -- this
/// call's own trap -- travelled to the caller's door as a foreign stash (a
/// re-run from entry or an `InternalError`). The SAM-named impl is also
/// accepted by its exact identity (`lambda_site_resolves_to`).
///
/// `true` (claim as before) with the switch off.
pub fn stash_reachable_from_site(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    if let Some(left_by) = cratonvm_jit::deopt::peek_last_deopt_left_by() {
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] callee service at {}.{}{} refuses an orphan left by {left_by}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return false;
    }
    if key_method != info.method_name {
        return true;
    }
    let Some(body_class) = cratonvm_jit::deopt::peek_last_deopt_trapping_body()
        .and_then(|body| body.published_class_id())
        .filter(|&id| id != 0)
        .map(ClassId::new)
    else {
        return true;
    };
    // JIT round 21 lane reflect4: the hidden-owner leaf's class by its id
    // (asked before the guard below, which it takes itself).
    let hidden_owner = jit_hidden_static_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let reachable = match info.invoke_kind {
        3 if hidden_owner.is_some() => hidden_owner == Some(body_class),
        0 | 2 if receiver_class_id.as_u32() != 0 => {
            receiver_class_id == body_class
                // JIT round 20 (lane stash): an id the store does not hold
                // (a lambda proxy) cannot be asked; fail-open.
                || cm.get_loader_id(receiver_class_id).is_none()
                || cm.is_subclass_of(receiver_class_id, body_class)
        }
        1 | 3 => {
            let site_class = if info.declaring_class_id == 0 {
                cm.find_bootstrap_class_by_name(info.class_name)
            } else {
                cm.find_cp_class_for_class(
                    info.class_name,
                    ClassId::new(info.declaring_class_id),
                )
            };
            site_class.is_none_or(|site| site == body_class || cm.is_subclass_of(site, body_class))
        }
        _ => true,
    };
    drop(cm);
    // The SAM-named lambda impl, by its exact identity (see the doc): asked
    // only on a refusal, after the class manager's read lock is released.
    let reachable = reachable
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !reachable && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] callee service at {}.{}{} refuses a stash whose trapping body's \
             class {} it cannot reach",
            info.class_name,
            info.method_name,
            info.descriptor,
            body_class.as_u32(),
        );
    }
    reachable
}



/// `CRATONVM_DBG_FORCE_CALLEE_DECLINE=<Class.method>` (j27 wave 8, lane W8ARG;
/// debug, unset by default): [`try_resume_trapped_callee`] declines every
/// stash whose outermost identity (`pkg/Class.method:(desc)R`) contains the
/// value, as it declines a redefined callee. The decline arms are otherwise
/// reached only by accident (a redefinition, a frame lost to a collection
/// during the build): the owned-arguments hand-off, whose caller's caller
/// re-runs the caller from bci 0 (`FramelessTrapSite::Handoff`, the two-level
/// rule `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY` included), and the
/// copying site's `rerun_declined_callee_from_entry`. A sound VM prints the
/// same facts with and without it (`tools/probes/jit/j27/J27W8ArgDeclineRows.java`).
/// Scope it to the probe's own methods: a filter without a `.` declines
/// nothing, so `=1` is inert. Read on the cold resume path only, no static.
pub fn forced_callee_decline(stash_key: &str) -> bool {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|filter| forced_decline_filter_names(&filter, stash_key))}

/// The pure half of [`forced_callee_decline`]: does `filter` name a method
/// (it contains a `.`) whose text `stash_key` contains?
pub fn forced_decline_filter_names(filter: &str, stash_key: &str) -> bool {
    let filter = filter.trim();
    filter.contains('.') && stash_key.contains(filter)
}

/// Zero the reference ARGUMENT words (not the receiver) of the compiled
/// caller's argument block `block` for `info`'s callee.
///
/// SAFETY: `block` is the live argument block of a compiled caller's frame
/// (the stub's `args_ptr`), whose thread is this one; nothing else reads it.
pub unsafe fn release_caller_reference_args(info: &JitInvokeInfo, block: &[i64]) {
    let first = usize::from(matches!(info.invoke_kind, 0 | 1 | 2));
    for (j, kind) in DescriptorParamIter::new(info.descriptor).enumerate() {
        if matches!(kind, b'L' | b'[') && first + j < block.len() {
            std::ptr::write_volatile(block.as_ptr().add(first + j) as *mut i64, 0);
        }
    }
}

/// SAFETY: same contract as the surrounding dispatch helpers — `vm` live,
/// `info` a live `JitInvokeInfo`, `thread` the current thread's exclusive
/// borrow (passed in, NOT re-acquired via `jit_thread_mut`, because some call
/// sites — `jit_invoke_virtual_mic` — hold the thread guard for their whole
/// body and a nested acquire would alias the `&mut`). `caller_block` is the
/// compiled caller's own argument block when the caller owns one to release.
pub unsafe fn try_resume_trapped_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    caller_block: Option<&[i64]>,
) -> Option<i64> {
    let trc = |why: &str, detail: &dyn std::fmt::Display| {
        if dbg_deopt_on() {
            eprintln!("[cratonvm-deopt] callee-resume refused ({why}): {detail}");
        }
    };
    // N2 — the OUTERMOST scope's identity, not the innermost.
    //
    // Every question this function asks below is about "the method this call
    // site invoked": does the stash name it, does its class resolve, is its bci
    // inside its code, which artifact's deopt points describe the trap. For a
    // frame with no caller chain — every frame in the tree today — that is the
    // stash's own `method_key` and the two peeks are the same call. For an
    // inlined chain it is the OUTERMOST scope: the innermost names a spliced
    // callee, which is never the name the call site was written with, so asking
    // the innermost would refuse every chain by construction and leave the
    // frame to travel outward as an orphan.
    let Some((key, bci)) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity() else {
        return None;
    };
    // `OmitStackTraceInFastThrow`: does the body that trapped stand for C2
    // code? Read now, while the stash still names it; only a resumed frame at
    // the trapping bytecode of such a body is a C2 throw site
    // (`ResumedCompiledFrameScope::enter_at`).
    let trapped_c2_tier = crate::runtime::exceptions::omit_stack_trace_in_fast_throw(vm)
        && cratonvm_jit::deopt::peek_last_deopt_trapping_body()
            .is_some_and(|body| crate::runtime::exceptions::body_is_c2_equivalent(&body));
    if key.is_empty() || bci == u32::MAX {
        trc(
            "no usable stash identity",
            &format_args!("key={key:?} bci={bci}"),
        );
        return None;
    }
    let Some((rest, key_desc)) = key.rsplit_once(':') else {
        trc("unparseable stash key", &key);
        return None;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        trc("unparseable stash key", &key);
        return None;
    };
    // j27 wave 8 (lane W8ARG): the debug lever that drives the decline arms
    // (the owned-arguments hand-off's caller re-run, the copying site's
    // callee re-run) on purpose; see [`forced_callee_decline`].
    if forced_callee_decline(&key) {
        trc("forced by CRATONVM_DBG_FORCE_CALLEE_DECLINE", &key);
        return None;
    }
    // A redefine of this exact callee invalidates the stashed frame. An
    // unrelated agent transformation must not disable deopt resume globally.
    //
    // Interpreter round i1 wave 18, lane L3: except the exit the safepoint
    // slow path granted a body compiled AFTER every redefinition of this VM
    // (`interpreter::granted_exit_of_a_current_body`). That body ran the
    // class's current bytecode, which is what the frame is rebuilt from
    // below; refusing it re-ran the callee from entry
    // (`rerun_declined_callee_from_entry`) and replayed what it committed.
    // Round 12 wave 2 (lane rt): a body compiled from the current bytecode
    // resumes (`stashed_callee_predates_redefinition`).
    let trap_point = cratonvm_jit::deopt::peek_last_deopt_point();
    // Interpreter round i1 wave 46, lane L2: the exit the safepoint verdict
    // granted a method-entry body forced to leave as its class's obsolete
    // activation after a renumbering redefinition
    // (`cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`).
    // Its frame is rebuilt in the bytecode the body was compiled from and
    // restamped with that bytecode's pool generation below, so it is neither
    // refused as a frame of a redefined class nor rebuilt from the class's
    // current bytecode. A single frame only (such a body leaves only at its
    // own code), of a method that holds no method monitor.
    let obsolete = crate::runtime::interpreter::granted_obsolete_activation_source(
        vm,
        cratonvm_jit::deopt::peek_last_deopt_cause(),
        trap_point,
        key_class,
        key_method,
        key_desc,
    )
    .filter(|(source, _)| {
        // Shape 1 (b) of the i41 page: a synchronized one only when the
        // stash carries the method monitor its self-locking body kept, which
        // the rebuilt frame then owns (as for a current body, below).
        cratonvm_jit::deopt::peek_last_deopt_frame(|f| {
            f.caller_frames.is_empty()
                && (!source.is_synchronized
                    || (cratonvm_jit::INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE
                        && (crate::runtime::interpreter::frame_hands_over_method_monitor(
                            vm,
                            source.declaring_class_id,
                            true,
                            source.is_static,
                            &source.code,
                            f,
                        ) || crate::runtime::interpreter::caller_holds_method_monitor(
                            vm,
                            thread,
                            source.is_static,
                            source.declaring_class_id,
                            f,
                        ))))
        })
        .unwrap_or(false)
    });
    // Round 14 wave 2 (lane deopt; proposal R13RP6-1): a SUPERSEDED body of
    // the redefined callee resumes in the bytecode it was compiled from when
    // its trampoline stashed that (`callsite_trap_own_source`), instead of
    // re-running from entry in the new bytecode. `Some((source, cp stamp))`.
    let own_source = if obsolete.is_none()
        && stashed_callee_predates_redefinition(
            vm,
            info.declaring_class_id,
            key_class,
            key_method,
            key_desc,
            trap_point,
        )
        && !crate::runtime::interpreter::granted_exit_of_a_current_body(
            vm,
            cratonvm_jit::deopt::peek_last_deopt_cause(),
            trap_point,
        )
        && !stash_is_from_a_current_body(vm, info, key_class, key_method, key_desc)
    {
        let class_id = {
            let cm = vm.classes.class_manager.read();
            if info.declaring_class_id == 0 {
                cm.find_bootstrap_class_by_name(key_class)
            } else {
                cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id))
            }
        };
        let own = class_id.and_then(|id| {
            crate::runtime::interpreter::callsite_trap_own_source(vm, id, key_method, key_desc)
        });
        if own.is_none() {
            trc("the stashed callee was redefined", &key);
            return None;
        }
        own
    } else {
        None
    };
    // The identity check, asked of the method the site RESOLVED to rather than
    // only of the name it was written with.
    //
    // For an ordinary call those are the same method, and `info` answers. For a
    // SAM call on a lambda they are NEVER the same: `info.method_name` is the
    // interface method (`apply`, `accept`, `test`) while the frame the compiled
    // body stashed names the synthetic impl (`lambda$static$0`). Comparing the
    // two therefore refused EVERY lambda callee's deopt, unconditionally and by
    // construction — the frame was dropped as an orphan and the impl was
    // re-entered from bci 0, re-running whatever it had already committed. See
    // `docs/feature-designs/deopt-osr.md`.
    //
    // The widening is exactly one alternative and it is not a relaxation: the
    // receiver must be a lambda proxy, its SAM must be the method this call
    // site names, and the impl the metafactory bound must be the stashed method
    // NAME FOR NAME. A foreign frame — the Groovy "duplicate `main`" shape that
    // forced the `5ceb880f` revert, and the reason this refusal is load-bearing
    // — satisfies none of those.
    if !(key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        && !lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc)
    {
        trc(
            "stash is not this call site's callee",
            &format_args!("stash={key} site={}{}", info.method_name, info.descriptor),
        );
        return None;
    }
    // HANDLER11-4 (JIT round 19 wave 12, lane handler12): and the body that
    // trapped must be one this site can reach (opt-in).
    if !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc) {
        trc(
            "stash's trapping body is not reachable from this call site",
            &format_args!(
                "stash={key} site={}.{}{}",
                info.class_name, info.method_name, info.descriptor
            ),
        );
        return None;
    }
    // A resume claimed through the lambda arm above is a lambda body's deopt
    // that did NOT re-run from entry, which is what the SAM door's own outcome
    // split counts. Record it there — the counter's question is "was a lambda
    // body's trapped frame spent or dropped", and the answer is the same
    // whichever door spent it. See `lambda_site_deopt_outcomes`.
    let via_lambda_identity = key_method != info.method_name;

    // Resolve the trapping method from ITS OWN declaring class (baked in the
    // key) — mirrors the `callee_compiler` resolution recipe.
    // Every refusal below is TRACED. They used to be bare `?` / `return None`
    // against a bound-but-unused `_resolve_trace`, so the five ways resolution
    // can decline were indistinguishable from "no stash" — and each one leaves
    // the frame stashed for an outer consumer that cannot attribute it, i.e.
    // each one MINTS an orphan. A refusal that cannot be named cannot be
    // counted, which is why the orphan in
    // `jit-direct-call-mints-an-orphaned-deopt-frame` was attributed to
    // inlining on no evidence.
    //
    // Wave 46 (lane L2): a renumbered obsolete activation's exit is rebuilt
    // in the bytecode its body was compiled from, not in the class's current
    // method, which a renumbering redefinition replaced.
    // R13RP6-1: an own-source resume already resolved the class and checked
    // the method against the body's own template.
    let cached = if let Some((source, _)) = &obsolete {
        std::sync::Arc::clone(source)
    } else if let Some((source, _)) = own_source.as_ref() {
        std::sync::Arc::clone(source)
    } else if let Some(template) = crate::runtime::interpreter::callsite_trap_current_template(
        vm, key_class, key_method, key_desc,
    ) {
        // Round 14 wave 3 (lane resume; proposal R14DP-1): the trampoline
        // named the trapping body, whose class was never redefined, so its
        // own template IS the method's current bytecode: no name resolution
        // through the call site's loader, no lock, no allocation.
        template
    } else {
        // JIT round 21 lane reflect4 (correctness, unconditional): the
        // hidden-owner leaf's callee (a spun `LambdaForm$MH`) by its id; by
        // name it never resolves, and the refusal left its frame an orphan.
        // Asked before the guard below, which `jit_substituted_owner` takes.
        let hidden_owner =
            jit_hidden_static_owner(vm, info).filter(|_| key_class == info.class_name);
        let lambda_call_site = vm.classes.lambda_call_site_for(receiver_class_id);
        // The published body id preserves loader identity when a call-site
        // loader cannot resolve an inlined callee's class name. The identity
        // and reachability gates above have already tied this stash to the site.
        let trap_body_class = cratonvm_jit::deopt::peek_last_deopt_trapping_body()
            .and_then(|body| body.published_class_id())
            .filter(|&id| id != 0)
            .map(ClassId::new);
        let cm = vm.classes.class_manager.read();
        let class_id = if let Some(owner) = hidden_owner {
            owner
        } else if info.declaring_class_id == 0 {
            match cm.find_bootstrap_class_by_name(key_class) {
                Some(id) => id,
                None => {
                    trc("stash class not found (bootstrap)", &key);
                    return None;
                }
            }
        } else {
            // The name is the declaring class of the method the call ran. For a
            // virtual or interface call that is the receiver's class or one of
            // its supertypes, which the RECEIVER's loader sees and the call
            // site's need not: a JDK `HashMap.hash` calling an application
            // class's `hashCode` is in the boot namespace, which cannot name
            // `org/springframework/core/ResolvableType`. The call site's loader
            // still answers first, so a static or special callee resolves as it
            // did.
            let from_site =
                cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id));
            let resolved = from_site
                .or_else(|| {
                    (receiver_class_id.as_u32() != 0
                        && receiver_class_id.as_u32() != info.declaring_class_id)
                        .then(|| cm.find_class_by_name_for_class(key_class, receiver_class_id))
                        .flatten()
                })
                .or_else(|| {
                    // JIT r23 wave 4b (lane L2; `CRATONVM_JIT_RESUME_HIDDEN_RECEIVER_CLASS`,
                    // page `r23-w4b-hidden-class-callee-trap-resume-by-name-FIXED-20261021.md`):
                    // a HIDDEN class is named by no loader, so neither lookup above finds
                    // it. When the stash names the receiver's own hidden class (a JDK-spun
                    // lambda forwarder's `apply` that trapped at its receiver null guard),
                    // the receiver's class id IS the callee's class. Without this the
                    // resume was refused and the call raised `InternalError` instead of
                    // re-executing the trapping instruction.
                    (receiver_class_id.as_u32() != 0)
                        .then(|| cm.get_class(receiver_class_id))
                        .flatten()
                        .filter(|c| c.is_hidden() && *c.name == *key_class)
                        .map(|_| receiver_class_id)
                })
                .or_else(|| {
                    trap_body_class.filter(|&id| {
                        cm.get_class(id).is_some_and(|class| *class.name == *key_class)
                    })
                })
                .or_else(|| {
                    // A lambda proxy has no loader of its own to ask, and the call site (a JDK
                    // `Predicate.test` caller in the boot namespace) cannot name an application
                    // class: `java.util.function.Predicate.test` on JUnit's
                    // `ReflectionUtils.lambda$findAllMethodsInHierarchy$37` was refused here and
                    // raised `InternalError` (the 12 `testBeginHandshakeCloseOutbound` invocations of
                    // `JdkSslEngineTest`). The lambda's own impl handle names the class, so when the
                    // receiver is a proxy whose SAM impl is the stashed method and exactly one loader
                    // defines that name, that definition is the callee's.
                    let call_site = lambda_call_site.as_ref()?;
                    let h = &call_site.impl_handle;
                    (&*h.class_name == key_class
                        && &*h.member_name == key_method
                        && &*h.descriptor == key_desc)
                        .then(|| cm.find_unique_class_by_name(key_class))
                        .flatten()
                });
            match resolved {
                Some(id) => id,
                None => {
                    trc(
                        "stash class not found from the call site's loader",
                        &format_args!("{key} from_class_id={}", info.declaring_class_id),
                    );
                    return None;
                }
            }
        };
        let store = cm.class_store();
        let Some((method, declaring_id)) =
            crate::classloading::find_method_recursive(class_id, key_method, key_desc, store)
        else {
            trc("stash method not found in its own class", &key);
            return None;
        };
        let Some(code_attr) = method.code() else {
            trc("stash method has no Code attribute", &key);
            return None;
        };
        let Some(declaring_class_name) = store.get(declaring_id).map(|c| &*c.name) else {
            trc("stash declaring class has no name", &key);
            return None;
        };
        // The key must name the method's OWN declaring class — a mismatch
        // means the name resolution drifted (e.g. class redefinition);
        // refuse rather than resume against different bytecode.
        if declaring_class_name != key_class {
            trc(
                "stash key names a different declaring class than resolution found",
                &format_args!("{key} resolved={declaring_class_name}"),
            );
            return None;
        }
        // Round 13 wave 9 (lane chain4): unless the stash carries the method
        // monitor its self-locking body kept at the guard, which the rebuilt
        // frame then owns as its `monitor_on_exit`
        // (`build_deopt_frame_or_refusal`); a frame this service cannot resume
        // is released as any compiled lock is (`CompiledLocksOfAStash`).
        //
        // Shape 2 (c) of the i41 page: or a caller-held synchronized CALL's
        // callee, whose monitor this thread holds for the caller; the rebuilt
        // frame then releases nothing of it and the caller's own edge does
        // (`caller_holds_method_monitor`).
        if method.is_synchronized()
            && !cratonvm_jit::deopt::peek_last_deopt_frame(|f| {
                crate::runtime::interpreter::frame_hands_over_method_monitor(
                    vm,
                    declaring_id,
                    true,
                    method.is_static(),
                    &code_attr.code,
                    f,
                ) || crate::runtime::interpreter::caller_holds_method_monitor(
                    vm,
                    thread,
                    method.is_static(),
                    declaring_id,
                    f,
                )
            })
            .unwrap_or(false)
        {
            trc("stash method is ACC_SYNCHRONIZED", &key);
            return None;
        }
        let num_params = crate::runtime::interpreter::count_method_params(key_desc);
        std::sync::Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: declaring_id,
                    class_name: std::sync::Arc::from(declaring_class_name),
                    method_name: std::sync::Arc::from(key_method),
                    method_descriptor: std::sync::Arc::from(key_desc),
                    source_file: store
                        .get(declaring_id)
                        .and_then(|c| c.source_file.as_deref())
                        .map(std::sync::Arc::from),
                    // Interned per method identity (no body copy per trap),
                    // as `resolve_callee_uncached` does.
                    code: crate::runtime::frame::padded_bytecode_for_method(
                        declaring_id,
                        key_method,
                        key_desc,
                        &code_attr.code,
                        &code_attr.exception_table,
                    ),
                    exception_table: std::sync::Arc::from(code_attr.exception_table.as_slice()),
                    max_stack: code_attr.max_stack,
                    max_locals: code_attr.max_locals,
                    num_params: num_params as u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: method.is_static(),
                },
            ),
        )
    };
    // `cached.code` carries 2 bytes of speculative-read padding, which are not
    // an instruction: a bci landing on them must be refused like one past the
    // end (it compared against the padded length, and admitted both).
    let code_len = cached.code.len().saturating_sub(2);
    if bci as usize >= code_len {
        trc(
            "resume bci past the method's code",
            &format_args!("{key} bci={bci} len={code_len}"),
        );
        return None;
    }

    // Commit: take the stash. From here every path either resumes or restores
    // it so nothing is silently dropped.
    // Round 14 wave 3 (lane resume; R14DP-3): with the body's own template,
    // which the restash below puts back.
    // `mut`: gch h2/fj's restash arm writes the pinned words back into it.
    let (mut rframe, stashed_cause, stashed_point, stashed_source) =
        cratonvm_jit::deopt::take_last_deopt_with_source()?;

    // That take may have discarded older frames beneath this one. They are
    // stale leftovers, never a deeper callee of this frame: the ordinary stash
    // admits re-execute points only, so an invoke at `bci` has not run, and a
    // callee that returned the sentinel never let this frame reach a later
    // guard. So nothing is refused on that account — see
    // `cratonvm_jit::deopt::deopt_stash_nesting_counts`.

    // Read BEFORE the build, for the refusal arm below: may a refused frame go
    // back into the stash? The stash rooted its `Object` words; taken out of
    // it, nothing does, and the build can collect (the virtual-object
    // materialiser allocates, the pool refill can GC).
    let gc_count_before = vm.mem.heap.collection_count();
    // Interpreter round i1 wave 23, lane L2: the locks the callee's compiled
    // code took, pinned below `pin_base` so every `truncate(pin_base)` leaves
    // them. A resumed frame and a restashed one keep them; the frame dropped
    // after a collection is re-run from entry, so its locks are released there.
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    let pin_base = thread.native_pin_roots.len();
    // gch h2/fj (`gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection`,
    // opt-in `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION`): root the taken frame's
    // own object words across the build, ABOVE `pin_base`, so every path that
    // truncates to `pin_base` (the resumed frame's push, the refusals) releases
    // them with the build's own pins. A single frame only: a chain's run keeps
    // what sits above `pin_base` until it returns.
    let restash_pinned = rframe.caller_frames.is_empty()
        && vm.jit.jit_cache.refused_resume_restashes_after_collection();
    let frame_pins = if restash_pinned {
        pin_taken_stash_frame_words(&mut thread.native_pin_roots, &rframe)
    } else {
        0
    };
    // N2 — the stash may name an inlined caller chain: the trap happened inside
    // a body this artifact spliced, so the callee's own frame sits beneath one
    // or more inlined scopes. `build_deopt_frame_inner` returns ONE frame by
    // value and cannot express that, so a chain is materialised through its own
    // entry point and run through its own tail below. Both are built before
    // anything is pushed or de-speculated, which keeps this function's
    // fail-before-commit discipline: a refusal here restores the stash and
    // leaves the caller exactly where it was.
    //
    // NOT LIVE TODAY. `jit/src/x64/inlining.rs`'s splice postcondition rolls
    // back any splice that published a deopt point, so no artifact carries a
    // chain; this is the consumer half landing first, so that relaxing the
    // postcondition does not hand a chain to a sink that raises
    // `InternalError: precise deoptimization unavailable`. See N2/N5 in
    // `NOTES-deopt.md`.
    enum Rebuilt {
        Single(crate::runtime::frame::Frame),
        /// Outermost first. Opaque: only
        /// `run_deopt_frame_chain_to_completion` may spend it.
        Chain(crate::runtime::interpreter::DeoptFrameChain),
    }
    let rebuilt = if rframe.caller_frames.is_empty() {
        match crate::runtime::interpreter::build_deopt_frame_or_refusal(
            vm, thread, &cached, &rframe, false, true,
        ) {
            Ok(frame) => Some(Rebuilt::Single(frame)),
            // Round 12 wave 5 (lane replay3): the callee's scalar-replaced
            // objects did not fit a full heap. HotSpot pops the callee and
            // throws `OutOfMemoryError` at this call; restashing or dropping
            // the frame here instead re-ran the callee from entry
            // (`service_frameless_callee_trap`), committing again what it had
            // committed. The build released the callee's compiled locks, so
            // `compiled_locks` (pinned below `pin_base`) only drops its pins.
            Err(why) if crate::runtime::interpreter::frame_build_heap_failure_is_answered(why) => {
                thread.native_pin_roots.truncate(pin_base);
                compiled_locks.unpin_keeping_the_locks(thread);
                trc(
                    "re-materialisation exhausted the heap; OutOfMemoryError at the call",
                    &format_args!("{key} bci={bci}"),
                );
                let _ = take_jit_deopt_pending();
                crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
                    vm,
                    &rframe,
                    stashed_cause,
                    stashed_point,
                );
                let oom = crate::runtime::interpreter::rematerialisation_out_of_memory(vm, thread);
                return Some(handle_jit_dispatch_error(vm, thread, oom, info));
            }
            Err(_) => None,
        }
    } else {
        // Round 13 wave 8 (lane chain3): by the trapping artifact's scope
        // class ids, found through the stashed point, with its redefinition
        // checks (`r13w8-chain3-helpers-callsite-chain-by-point-patch`).
        crate::runtime::interpreter::build_deopt_frame_chain_for_point(
            vm,
            &cached,
            &rframe,
            stashed_point,
        )
        .map(Rebuilt::Chain)
    };
    let Some(rebuilt) = rebuilt else {
        // gch h2/fj (d10f restash): a collection ran during the build, so the
        // frame's words may name from-space; read them back from their pins
        // (rewritten in place by the collector) BEFORE the truncate below.
        let collected = vm.mem.heap.collection_count() != gc_count_before;
        let refreshed = collected
            && restash_pinned
            && refresh_taken_stash_frame_words(
                &thread.native_pin_roots,
                pin_base,
                frame_pins,
                &mut rframe,
            );
        // Unmappable — release partial pins, restore the stash, and let
        // the sentinel propagate to the outer (identity-checked) sinks.
        thread.native_pin_roots.truncate(pin_base);
        trc("frame not materialisable", &format_args!("{key} bci={bci}"));
        // EXCEPT when a collection ran during the build
        // (r11-tier-deopt-resume-gc-windows, helpers half). Every refusal that
        // does not depend on an allocation is asked before the materialiser
        // and the pool refill, so what is left is an allocation that FAILED
        // after shells were allocated (OOM, which collects before it fails)
        // and the unreachable post-refill `StackPush`. Then `rframe`'s
        // `Object` words may name from-space, and restashing them hands the
        // next sink stale oops. Drop the frame instead: the sentinel
        // propagates without it and the caller takes the re-run. A refusal
        // with no collection in between restashes exactly as before, so the
        // outer sinks still see (and charge) an ordinary unmappable frame.
        // gch h2/fj: unless the frame's words were pinned across the build
        // and read back above (`refreshed`): then it is current and restashed
        // as after a refusal with no collection.
        if collected && !refreshed {
            trc(
                "refused after a collection; the frame is dropped, not restashed",
                &format_args!("{key} bci={bci}"),
            );
            // The frame is gone and the callee re-runs from entry: release
            // what its compiled code locked, through the pins (the collection
            // may have moved the objects).
            compiled_locks.release_for_a_rerun(vm, thread);
            // Leave a FRAMELESS trap behind, not a bare sentinel. A trampoline
            // stash (`x64_deopt_entry`, `ir_deopt_entry`) raises no deopt flag,
            // so with the frame gone nothing would mark the `i64::MIN` as a
            // trap: `jit_dispatch_threw` would answer 0 and a `J`/`D` call site
            // would keep it as a genuine `Long.MIN_VALUE` (round 11 wave 12,
            // the residual of r11w12-tier-helpers-declined-callee-rerun-patch).
            // With the flag, the dispatch door's frameless arm re-runs the
            // callee and every other sink treats the sentinel as a deopt.
            //
            // Round 13 wave 8 (lane replay4): judged here, while the frame's
            // bci still exists, so the service's replay verdict is the
            // prefix rule and not the whole-body one (which would refuse a
            // re-run that commits nothing twice). The verdict reads no
            // object word of the frame.
            // R13RP6-1: an own-source frame's bci is in the OLD bytecode, and
            // its re-run runs the new one: never exact.
            let exact = own_source.is_none()
                && callee_rerun_replay_is_exact(
                    vm,
                    info.declaring_class_id,
                    key_class,
                    key_method,
                    key_desc,
                    Some((&rframe, stashed_point)),
                )
                .unwrap_or(false);
            set_jit_deopt_pending();
            note_dropped_callee_frame(exact);
            return None;
        }
        if refreshed {
            // The positive control of `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION`.
            trc(
                "refused after a collection; restashed at its pinned addresses",
                &format_args!("{key} bci={bci} words={frame_pins}"),
            );
        }
        // Back into the stash, whose next sink decides about the locks.
        compiled_locks.unpin_keeping_the_locks(thread);
        // R14DP-3: the next sink keeps the name of the body that trapped
        // (`CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE=0` drops it, as before).
        let stashed_source = stashed_source.filter(|_| {
            true
        });
        cratonvm_jit::deopt::restash_last_deopt_with_source(
            rframe,
            stashed_cause,
            stashed_point,
            stashed_source,
        );
        return None;
    };

    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] helper precise-resume of trapped callee {key} at bci={bci}{}",
            if own_source.is_some() {
                " (in the bytecode its superseded body was compiled from)"
            } else {
                ""
            }
        );
    }
    if via_lambda_identity {
        crate::runtime::interpreter::lambda_site_bump_resumed();
    }
    // Committed: this trap is serviced here, so the deopt flag a
    // frame-stashing `jit_uncommon_trap` raises beside its stash is retired
    // with it. Left set, it outlives the call — the compiled caller continues
    // on the resumed value and a later legitimate `Long.MIN_VALUE` return reads
    // it as a sentinel (see `route_implicit_exc_through_callee`'s serviced
    // arm). A throw out of the resumed frame parks its own pending exception.
    let _ = take_jit_deopt_pending();

    // De-speculate the trapping method FIRST (record + evict + escalate), so
    // repeated traps stop re-entering the artifact. The policy — and the
    // reason it is not a plain `deoptimize` call — lives in
    // `despeculate_trapped_method`, which the tier-up sink in
    // `interpreter.rs` asks too so the two cannot answer differently.
    //
    // N3(b): the fallback reason used to be a hard-coded `UnreachedCode`, on
    // the SUCCESS path — this is the precise resume of a trapped callee that
    // the whole machinery above exists to produce. `UnreachedCode` is the one
    // reason `recommend_action` answers `MakeNotCompilable` on the FIRST
    // occurrence, so a single trap of any kind permanently retired the callee,
    // whatever actually trapped. The real reason is on the
    // `DeoptimizationPoint` the stub baked; read it from the callee's own
    // artifact, and reserve `UnreachedCode` for genuinely unreachable code.
    // The frame-aware rule only for a single frame: a chain's `rframe.bci` is
    // the INNERMOST scope's, while `bci` and the artifact are the outermost's.
    // The point that fired names the artifact that trapped. When the cache
    // publishes a DIFFERENT body for this method, the trap came out of a body
    // an earlier trap already retired (a stale call-site cache kept calling
    // it). Charging it would evict the fresh body BY NAME and count the one
    // speculation failure again, N callers N times -- the helper-sink half of
    // r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
    // `trapping_artifact_is_superseded`'s rule. An empty slot and an unknown
    // point (0) keep the old charge. The resume below still runs.
    // R13RP6-1: an own-source resume is by construction a superseded body's
    // trap (compiled before the redefinition), whatever the slot holds now.
    // Interpreter round i2 wave 61, lane L2
    // (`i59-L2-proposal-every-stash-names-its-trapping-body`): the published
    // body is compared with the one the trampoline named, by POINTER, when
    // the stash named one; by the point's address otherwise.
    //
    // One method, two cache keys: an inherited method compiled for a receiver
    // class is published under THAT class's name (`ISO_8859_1$Decoder.decode`
    // beside `CharsetDecoder.decode`, both the one bytecode), and the key this
    // sink read from the stash is the declaring class's. Asked of the declaring
    // key alone, the receiver-keyed body is "a different body than the
    // published one" for as long as the process lives -- never charged, so
    // never evicted, and (published) never made not entrant: `TestMethodPerformance`'s
    // `MessageBytes` loop resumed `CharsetDecoder.decode(BB,CB,Z)` at bci 54 in
    // the interpreter on about three iterations in four. A body the stash
    // named, that its OWN key still publishes, is the live body of that key,
    // not a superseded one. Its trap is charged under the declaring class's
    // key (the one the log, the tiered manager and a recompile's de-spec read),
    // with the reason read from THAT body, and the body is evicted under its own
    // key.
    // j27 wave 7 (lane T1LAM3): the named body and the cache's handle are
    // `RetainedCode`s (`taken_trapping_body_retained`): the stash's `Weak`
    // upgrade may be the body's last owner.
    let named_trapping_body: Option<cratonvm_jit::RetainedCode> = (stashed_point != 0
        && cratonvm_jit::deopt::stash_names_trapping_body_enabled())
    .then(|| stale_trap_not_entrant::taken_trapping_body_retained(stashed_point))
    .flatten();
    let sibling_key: Option<(std::sync::Arc<str>, ClassId, cratonvm_jit::RetainedCode)> =
        own_source
            .is_none()
            .then(|| named_trapping_body.clone())
            .flatten()
            .and_then(|named| {
            let (class, method, descriptor, class_id) = named.published_key()?;
            if &*method != key_method || &*descriptor != key_desc {
                return None;
            }
            if &*class == key_class && class_id == cached.declaring_class_id.as_u32() {
                return None;
            }
            let held = vm
                .jit
                .jit_cache
                .read()
                .get(&class, &method, &descriptor, ClassId::new(class_id))
                .map(cratonvm_jit::RetainedCode::new)?;
            std::sync::Arc::ptr_eq(held.arc(), named.arc())
                .then(|| (class, ClassId::new(class_id), named))
        });
    if let Some((class, _, _)) = &sibling_key {
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] {key}: the trapping body is published under {class}, not under \
                 the declaring class's key: charged once under the declaring key, evicted under its own"
            );
        }
    }
    let trapped_a_superseded_body = own_source.is_some()
        || (stashed_point != 0
            && vm
                .jit
                .jit_cache
                .read()
                .get(key_class, key_method, key_desc, cached.declaring_class_id)
                // j27 wave 7 (lane T1LAM3): both handles are `RetainedCode`s.
                .map(cratonvm_jit::RetainedCode::new)
                .is_some_and(|live| {
                    let by_address = !live.owns_deopt_point(stashed_point);
                    match named_trapping_body.as_ref() {
                        Some(named) => {
                            // A sibling (published under its own key) is not
                            // superseded: it is charged below.
                            let superseded = !std::sync::Arc::ptr_eq(live.arc(), named.arc())
                                && sibling_key.is_none();
                            if dbg_deopt_on() {
                                eprintln!(
                                    "[cratonvm-deopt] callee trap body named by the stash \
                                     (superseded): {key} superseded={superseded} \
                                     by-address={by_address} agrees={} named@{:p} \
                                     not-entrant={} live@{:p} not-entrant={} chain-frames={} fired=[{}]",
                                    superseded == by_address,
                                    named.entry_ptr(),
                                    named.is_not_entrant(),
                                    live.entry_ptr(),
                                    live.is_not_entrant(),
                                    rframe.caller_frames.len(),
                                    named
                                        ._deopt_point_boxes
                                        .iter()
                                        .map(|b| &**b)
                                        .chain(named.deopt_points.iter())
                                        .find(|p| *p as *const cratonvm_jit::deopt::DeoptimizationPoint as usize == stashed_point)
                                        .map(|p| format!("reason={:?} action={:?} spec={} bci={} sem={:?}", p.reason, p.action, p.speculation_id, p.bci, p.semantics))
                                        .unwrap_or_default()
                                );
                            }
                            superseded
                        }
                        None => by_address,
                    }
                }));
    if trapped_a_superseded_body && dbg_deopt_on() {
        eprintln!("[cratonvm-deopt] {key} bci={bci}: trap from a superseded body, not charged");
    }
    // j27 wave 7 (lane T1LAM3, t1-X2 page; `CRATONVM_JIT_STALE_TRAP_NOT_ENTRANT`,
    // opt-in): a baked call still enters that stale body, so make IT not
    // entrant, as an eviction does (`stale_trap_not_entrant`).
    if trapped_a_superseded_body && stale_trap_not_entrant::enabled() {
        if let Some(stale) = stale_trap_not_entrant::body_to_patch(
            vm,
            stashed_point,
            own_source.is_some(),
            sibling_key.is_some(),
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
        ) {
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] stale-body-trap {key} bci={bci}: body id={} made not entrant",
                    stale.compile_id
                );
            }
            make_trap_evicted_body_not_entrant(vm, &key.to_string(), stale);
        }
    }
    // Round 16 wave 15 (lane trapfix; FS14-4): an empty slot whose method an
    // earlier trap already declared not compilable is charged ONCE, by that
    // earlier trap. A later trap can only come out of the evicted body
    // through a binding the eviction did not withdraw; charging it again
    // re-ran the whole `MakeNotCompilable` decision per failing call.
    let trapped_a_superseded_body = trapped_a_superseded_body
        || trap_from_a_body_already_declared_not_compilable(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            bci,
        );
    // Interpreter round i1 wave 15, lane L3: a callee that left at a
    // back-edge poll because the method must now run interpreted is resumed
    // like any trap, and not charged. Asked of the stash's own cause, so an
    // artifact the cache no longer holds (whose reason
    // `trapped_callee_deopt_reason` could not read) is not charged as an
    // `UncommonTrap` either.
    let left_for_the_interpreter = stashed_cause.is_some_and(|c| {
        crate::runtime::interpreter::exit_left_for_the_interpreter(
            vm,
            c.reason,
            cached.declaring_class_id,
            key_method,
            key_desc,
        )
    });
    // Round 15 wave 4 (lane chain5): a forced chain exit of
    // `CRATONVM_DEOPT_EAGER_CHAINS` (claimed by the chain's build,
    // `build_deopt_frame_chain_for_point`) is a passing guard, not charged
    // under `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`.
    let uncharged_forced = !trapped_a_superseded_body
        && !left_for_the_interpreter
        && crate::runtime::interpreter::leave_forced_trip_uncharged(
            vm,
            matches!(&rebuilt, Rebuilt::Chain(chain) if chain.is_forced_trip()),
        );
    // JIT round 19 wave 4 (lane splice4, SPLICE3-2,
    // `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS`): a marked
    // expected-throw trip of the live body is resumed below and not charged,
    // as the door sink leaves it (`deopt_resume::stash_sink_expected_throw_uncharged`;
    // the live body is the one that fired: a superseded one is not charged
    // anyway). Off: `false`, today's charge.
    let expected_throw = !trapped_a_superseded_body
        && !left_for_the_interpreter
        && !uncharged_forced
        && cratonvm_jit::ir::ir_splice_expected_throw_stash_sinks_enabled()
        && {
            let live = vm.jit.jit_cache.read().get(
                key_class,
                key_method,
                key_desc,
                cached.declaring_class_id,
            );
            crate::runtime::interpreter::stash_sink_expected_throw_uncharged(
                live.as_deref(),
                &rframe,
            )
        };
    if expected_throw && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {key} bci={bci}: expected throw in a spliced {} bci={} \
             (helper sink, CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS), not charged",
            rframe.method_key, rframe.bci
        );
    }
    if !trapped_a_superseded_body
        && !left_for_the_interpreter
        && !uncharged_forced
        && !expected_throw
    {
        let frame_for_reason = rframe
            .caller_frames
            .is_empty()
            .then_some((&cached.code[..], &rframe));
        // Round nio21 wave 7 (lane L2,
        // `r21-callsite-service-chain-trap-despec-grain-differs-from-the-doors-OBSOLETE-20261012.md`):
        // an inlined chain's trap is charged as the doors charge it
        // (`deopt_resume::stash_charge_site` / `stash_trap_attribution`):
        // at the call site `bci` in this method, under the STASH's own reason
        // (else `UncommonTrap`), de-spec'd there with the wildcard. The
        // artifact's points at `bci` describe the call site, not the spliced
        // guard that fired: an optimizing body publishes a plain
        // `TransferToInterpreter` resume point there, whose `Reinterpret`
        // never evicted the body, and whose derived id withdrew nothing the
        // splice planner asks about. Default ON
        // (`CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE=0` turns it off).
        //
        // JIT round 19 wave 10, lane deopt10 (`i62-L2-proposal-single-pass-
        // chain-points-leave-the-roots-bci-key-space`, opt-in
        // `CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS`) charges the same reason
        // independently; with the nio21 switch off it still applies. Off both:
        // as before.
        let chain_charge = chain_trap_charge(&rframe, stashed_cause);
        let trap_reason = match chain_charge {
            Some(c) => c.reason,
            None if frame_for_reason.is_none()
                && cratonvm_jit::osr_exit::chain_points_off_root_bcis_enabled() =>
            {
                chain_trap_charge_reason(stashed_cause)
            }
            None => trapped_callee_deopt_reason(
                vm,
                cached.declaring_class_id,
                key_class,
                key_method,
                key_desc,
                bci,
                frame_for_reason,
                stashed_cause,
                stashed_point,
                sibling_key.as_ref().map(|(_, _, body)| body.clone()),
            ),
        };
        // nio21 wave 20, lane L4 (`CRATONVM_DEOPT_CAUSE_FROM_GUARD_BOXES`): a
        // single-frame trap attributed to the table's uncharged
        // `TransferToInterpreter` resume point while the point that FIRED (by
        // its stashed address, in the live body the charge is about) is a
        // guard with its own reason is charged under that reason. The w19b
        // rule covers a stash that carries its cause; this one covers a stash
        // that carries only the point.
        let trap_reason = if chain_charge.is_none() && stashed_point != 0 {
            let live = vm.jit.jit_cache.read().get(
                key_class,
                key_method,
                key_desc,
                cached.declaring_class_id,
            );
            crate::runtime::interpreter::nio21_charge_reason_from_fired_point(
                trap_reason,
                live.as_deref().and_then(|l| {
                    crate::runtime::interpreter::nio21_fired_point_reason(l, stashed_point)
                }),
            )
        } else {
            trap_reason
        };
        // Charged ONCE, under the declaring class's name: that is the key the
        // deoptimization log, the tiered manager and the optimizing compile's
        // de-spec registry (`despec_method_key`) read, so the per-bci
        // withdrawal reaches the next body and the per-method trap budget moves
        // by one trap per trap. Recorded under the receiver class's name only,
        // the withdrawal was never seen and the next body carried the same
        // failing guard; under both names the budget ran out twice as fast.
        // Wave 4 (lane splice4, SPLICE3-2): the frame-aware form, with the
        // nio21 wave 7 chain grain; both switches off, exactly
        // `despeculate_trapped_method`.
        // JIT round 21 wave 1 (lane loops, LOOPS21-2,
        // `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, opt-in): a loop-entry
        // predicate's trap is withdrawn under its planter's own id (the
        // point's, `predicate_trap_charge`), not the wildcard this sink
        // records for an unnamed `UncommonTrap`. Off: `chain_charge`, as before.
        let despec_as = chain_charge
            .or_else(|| predicate_trap_charge(&rframe, bci, stashed_cause, trap_reason));
        despeculate_trapped_method_of_frame_despec_as(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            trap_reason,
            bci,
            &rframe,
            despec_as,
        );
        // The eviction, though, is the sibling key's: the controller above
        // removed whatever the DECLARING key publishes, and the body that
        // trapped is published under its own. Withdrawn as the controller
        // withdraws a body (removed, then made not entrant), under the
        // controller's own predicate, and only while the key still publishes
        // that very body.
        if let Some((sibling_class, sibling_id, sibling_body)) = &sibling_key {
            if cratonvm_jit::tiered::deopt_evicts_method_body(
                trap_reason,
                cratonvm_jit::deopt::DeoptAction::Reinterpret,
            ) {
                let evicted = {
                    let mut jit_cache = vm.jit.jit_cache.write();
                    let held = jit_cache.get(sibling_class, key_method, key_desc, *sibling_id);
                    match held {
                        Some(held) if std::sync::Arc::ptr_eq(&held, sibling_body.arc()) => {
                            jit_cache.remove(sibling_class, key_method, key_desc, *sibling_id);
                            Some(held)
                        }
                        other => {
                            // Not the body that trapped: left in place.
                            if let Some(other) = other {
                                drop(cratonvm_jit::RetainedCode::new(other));
                            }
                            None
                        }
                    }
                };
                if let Some(evicted) = evicted {
                    if trap_eviction_not_entrant_enabled() {
                        make_trap_evicted_body_not_entrant(vm, &key.to_string(), evicted);
                    } else {
                        drop(cratonvm_jit::RetainedCode::new(evicted));
                    }
                }
            }
        }
        // r9w10 exc10: a zero divisor that keeps exiting the optimizing tier.
        // JIT round 19 wave 12, lane tail12 (TAIL11-1, opt-in
        // `CRATONVM_JIT_DIV_GUARD_RETIER_CHAINS`): for an inlined CHAIN the
        // division is the TRAPPING scope's (`bci` is the outer invoke), and
        // its reason is the stash's own cause (the outer artifact's points at
        // the invoke describe the call site). Off: the outer invoke's opcode,
        // which is never a division, so a spliced division never re-tiered.
        let (retier_reason, retier_opcode) = match &rebuilt {
            Rebuilt::Chain(chain) if true => (
                chain_trap_charge_reason(stashed_cause),
                chain.trapping_scope_opcode(),
            ),
            _ => (trap_reason, cached.code.get(bci as usize).copied()),
        };
        retier_hot_division_guard_trap(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            retier_reason,
            retier_opcode,
            bci,
        );
    }

    // ARGFIX: the rebuilt frame owns the arguments now, and the resume is past
    // every decline that would re-run the callee from entry off the caller's
    // copy. That copy is named in the caller's oop map for the whole resumed
    // run, so a reference the callee drops would stay reachable. Receiver kept.
    if let Some(block) = caller_block {
        release_caller_reference_args(info, block);
    }

    // Run the reconstructed frame to completion. A single frame's pins are
    // released as soon as it is pushed (gen r5w4/jit8,
    // `execute_resumed_frame_releasing_pins`): they used to stay installed for
    // the whole run, "over-rooting is harmless" -- harmless for safety, but it
    // kept every reference the trapped frame held at its deopt point alive
    // until the resumed method returned, whatever it dropped meanwhile. The
    // truncate after the match stays: it releases what a chain's path or a
    // leaked nested scope left above `pin_base`.
    //
    // For a chain the same thing happens one level up: every frame is pushed
    // outermost-first and the whole chain runs until the OUTERMOST frame — the
    // callee this dispatch helper actually invoked — returns. That is the value
    // the compiled caller is waiting for. See
    // `run_deopt_frame_chain_to_completion` for why the depth it measures
    // against cannot be `execute_prebuilt_frame`'s.
    let res = match rebuilt {
        Rebuilt::Single(frame) => {
            // r9w9 exc9: the resumed frame IS the trapped compiled callee, so an
            // implicit exception its own re-executed bytecode raises (the zero
            // divisor that trapped it) is a compiled-code throw for
            // `OmitStackTraceInFastThrow` (opt-in, default off). The frame will
            // sit at `len() + 1` once pushed, at `bci`, the trapping bytecode
            // (a single frame: the outermost identity is the innermost).
            let _fast_throw_scope = crate::runtime::exceptions::ResumedCompiledFrameScope::enter_at(
                thread.frames.len() + 1,
                bci,
                trapped_c2_tier,
            );
            // No MethodEntry: the callee was entered in compiled code.
            match (&obsolete, own_source.as_ref()) {
                // Wave 46 (lane L2): restamped with the body's pool generation
                // after the push and before its first bytecode, then moved onto
                // its translated body, as the interpreter doors' sink does.
                (Some((_, stamp)), _) => {
                    if dbg_deopt_on() {
                        eprintln!(
                            "[cratonvm-deopt] {key} bci={bci}: renumbered obsolete activation \
                             resumed by the call-site service in the bytecode its body was \
                             compiled from"
                        );
                    }
                    crate::runtime::interpreter::execute_rebuilt_frame_releasing_pins(
                        vm,
                        thread,
                        frame,
                        pin_base,
                        Some(*stamp),
                    )
                }
                // R13RP6-1: restamped with the body's pool generation.
                (None, Some(&(_, cp_stamp))) => {
                    crate::runtime::interpreter::execute_own_source_frame_releasing_pins(
                        vm, thread, frame, pin_base, cp_stamp,
                    )
                }
                (None, None) => crate::runtime::interpreter::execute_resumed_frame_releasing_pins(
                    vm, thread, frame, pin_base,
                ),
            }
        }
        Rebuilt::Chain(chain) => {
            let depth = chain.frame_count();
            // gcd d1/b: the chain's pins end at its push too, as the single
            // frame's do (`run_deopt_frame_chain_to_completion_releasing_pins`).
            match crate::runtime::interpreter::run_deopt_frame_chain_to_completion_releasing_pins(
                vm, thread, chain, pin_base,
            ) {
                Some(res) => res,
                None => {
                    // `push_inlined_chain` only fails on an operand-stack push
                    // that materialisation already bounded against the frame's
                    // own `max_stack`, so this is unreachable by construction —
                    // but if it ever happens, some frames of the chain ARE
                    // already on the stack, so "restore the stash and let the
                    // caller re-run" would run them twice. REFUSE loudly
                    // instead: the interpreter unwinds the pushed frames the
                    // way it unwinds any error, and no side effect is replayed.
                    thread.native_pin_roots.truncate(pin_base);
                    compiled_locks.unpin_keeping_the_locks(thread);
                    return Some(handle_jit_dispatch_error(
                        vm,
                        thread,
                        crate::error::MethodCallFailed::InternalError(
                            crate::error::VmError::Internal {
                                message: format!(
                                    "deopt resume could not push a {depth}-frame inlined chain \
                                     for {key} at bci={bci}; refusing to re-execute the invoke"
                                ),
                            },
                        ),
                        info,
                    ));
                }
            }
        }
    };
    thread.native_pin_roots.truncate(pin_base);
    // The resumed frame held the locks and released them itself.
    compiled_locks.unpin_keeping_the_locks(thread);

    Some(match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    })
}

/// Inline capacity shared with `safe_native_call_impl`. It covers receiver
/// plus the full x64 register envelope and keeps ordinary JIT-to-native
/// dispatch free of temporary heap allocations.
pub const INLINE_JIT_NATIVE_ARGS: usize = 8;
pub type JitDecodedArgs = smallvec::SmallVec<[Value; INLINE_JIT_NATIVE_ARGS]>;

/// Decode a JIT dispatch helper's raw `i64` argument slice into the
/// `Value` slice the interpreter expects. Centralised so that the slow
/// path in `jit_invoke_dispatch` and the three `try_call_compiled_entry`
/// overflow bailouts (DISPATCH_CACHE hit, JIT-cache hit, post-compile)
/// all reconstruct args the same way — round-5 CRIT-1 fix.
///
/// `invoke_kind` matches the JIT calling-convention encoding: 0/1/2 are
/// virtual/static/special with an explicit receiver in `args_slice[0]`;
/// 3 is the no-receiver form (used for static and indy callees that the
/// JIT emits without a leading `this` slot).
///
/// SAFETY: `args_slice` must be a slice of valid `i64` arg slots produced
/// by the JIT caller. `vm` must be a live `SharedVm`; the heap is queried
/// to validate any potential object pointers before round-tripping them
/// through `ObjectRef`.
#[inline]
/// Does a call site of this [`JitInvokeInfo::invoke_kind`] pass a RECEIVER in
/// argument slot 0?
///
/// `0` virtual, `1` special, `2` interface, `3` static, `4` self-recursive
/// static DIRECT call. Slots 0–2 carry a receiver; **3 and 4 do not** — kind 4
/// is a *static* call, the IR backend's marker for "emit a raw `CALL` to this
/// method's own entry" rather than a different dispatch shape.
///
/// # Why this is a named predicate and not `!= 3`
///
/// It was `!= 3`, written when 4 did not exist, and
/// [`JitInvokeInfo::invoke_kind`]'s own doc explains why that looked safe:
/// "such a call NEVER reaches the dispatch helper, so the helper need not
/// handle 4". True of `jit_invoke_dispatch`. Not true of the DEOPT-SERVICING
/// door — `jit_service_callee_deopt` → `handle_compiled_callee_deopt_sentinel`
/// → `try_run_callee_handler` → [`decode_dispatch_values`] — which a direct
/// self-`CALL` reaches on every sentinel return, and which rebuilds the
/// callee's interpreter frame from these very values.
///
/// So kind 4 took the receiver branch: argument 0 (an ordinary `int`, `long`,
/// whatever the method's first parameter is) was consumed as `this`,
/// `is_object_address` rejected it, and it became `Value::Object(None)` —
/// **and every remaining argument shifted down one slot**. The rebuilt handler
/// frame's local 0 was then a null reference instead of the method's first
/// parameter.
///
/// MEASURED on `dev` 7281ce4b6, a self-recursive `static long f(int)` whose own
/// body catches what its recursive call throws implicitly, reading the result
/// from INSIDE the compiled caller (10,000 calls each):
///
/// | bottom-of-recursion trap | `--nojit` / `C2_ACCEPT=never` | default | `always` |
/// | --- | --- | --- | --- |
/// | `NUL.hashCode()` (NPE)      | 0 wrong | **9,941** | **9,944** |
/// | `EMPTY[3]` (AIOOBE)         | 0 wrong | **9,940** | **9,944** |
/// | `1 / 0` (ArithmeticException) | 0 wrong | 17–226 | ~228 |
/// | `throw new IllegalStateException` (control) | 0 wrong | 0 | 0 |
///
/// The handler returned `-0` where it owed `-1`: `catch (…) { return -n; }`
/// read `n` out of a local the decoder had replaced with null. The explicit
/// `throw` row is the control — it never reaches this decoder — and the two
/// interpreted arms are the oracle.
///
/// Every OTHER `invoke_kind` test in this file already spells the set it
/// means (`matches!(info.invoke_kind, 0 | 2)` for receiver-CLASS resolution,
/// `matches!(info.invoke_kind, 1 | 3)` for statically bound), which is why
/// this one is a predicate now rather than a fourth spelling.
#[inline]
pub fn invoke_site_has_receiver(invoke_kind: u8) -> bool {
    matches!(invoke_kind, 0 | 1 | 2)
}

/// Does the compact argument buffer of `info`'s site carry a receiver in word
/// 0? [`invoke_site_has_receiver`] for kinds 0-3; for the IR's direct
/// self-call row (kind 4) the row's own shape: a static self-call has none,
/// and a receiver-carrying one (JIT round 23 wave 3d,
/// `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`; the VM is normally handed
/// its kind-1 twin) has one exactly when `num_jit_args` is one more than the
/// descriptor's argument count (`cratonvm_jit::self_call_info_has_receiver`).
/// The one rule the reference walk ([`for_each_jit_reference_arg`]) and the
/// argument decoder ([`decode_dispatch_values_into_shaped`]) share.
#[inline]
pub fn jit_info_has_receiver(info: &JitInvokeInfo) -> bool {
    invoke_site_has_receiver(info.invoke_kind) || cratonvm_jit::self_call_info_has_receiver(info)
}

pub unsafe fn decode_dispatch_values(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> JitDecodedArgs {
    let mut values = JitDecodedArgs::new();
    decode_dispatch_values_into(vm, info, args_slice, None, &mut values);
    values
}

/// [`decode_dispatch_values`] writing into a caller-owned buffer, with an
/// optional already-resolved receiver.
///
/// Two costs the by-value form carried, both measured in
/// `jit_native_dispatch_profile`:
///
///  * **`SmallVec::with_capacity`.** `JitDecodedArgs` is a
///    `SmallVec<[Value; 8]>` — eight 16-byte `Value`s plus a capacity word,
///    ~144 bytes. The zero-argument rung (`decode: static, ()J`) measured
///    **10.5 ns** with no argument work to do at all, and building the buffer
///    with `new()` in a caller-owned local took the same rung to **0.2 ns**.
///    The first guess was the return move; it was not — `with_capacity` is an
///    outlined call, so the whole value has to be materialised in memory for
///    it, while `new()` inlines and the buffer stays in registers for a decode
///    this small. Worth recording because the by-value shape reads like the
///    expensive part and is not.
///  * **A third validation of the receiver.** `try_jit_site_cached_native_dispatch`
///    resolves the receiver through `is_object_address` to guard the site, and
///    passes the resolved `ObjectRef` here rather than handing back the raw word
///    for this function to validate again. `None` keeps the original behaviour
///    for callers that have not resolved one.
#[inline]
pub unsafe fn decode_dispatch_values_into(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver: Option<Option<ObjectRef>>,
    values: &mut JitDecodedArgs,
) {
    decode_dispatch_values_into_shaped(vm, info, args_slice, receiver, None, 0, values)
}

