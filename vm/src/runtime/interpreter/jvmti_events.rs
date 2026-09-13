// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI event delivery from the interpreter.
//!
//! The seven `fire_jvmti_*` sites plus `push_frame_and_fire_entry`, which
//! is the one that has to fire *and* push in the right order — an agent
//! that receives MethodEntry before the frame exists sees a stack that
//! does not contain the method it was told about.
//!
//! Worth collecting for a documented reason: a JVMTI delivery lane's
//! handover census said ~15 call sites in `interpreter.rs`. The real
//! count was 28, and 8 of them were in `invoke.rs` — a file the census
//! never mentioned. Trusting it would have left the cached-invoke and OSR
//! surface unattributed while looking complete. Delivery sites are now
//! countable by opening one file.

use super::site_cache::site_stats;
use super::*;

// ---------------------------------------------------------------------------
// JVMTI ExceptionCatch hook
// ---------------------------------------------------------------------------

/// Fire the JVMTI `ExceptionCatch` event when an exception-table lookup
/// resolves to a matching handler. Zero-cost (single atomic load + branch)
/// when no agent is attached — the runtime JVMTI free function gates on the
/// global manager's fast-path flag before doing any further work.
///
/// The `MethodId` is synthesized from the frame's class id and the first 32
/// bits of an FNV hash of the method name. This matches the scheme used by
/// the `VmClassMethodProvider` in `vm/src/jvmti/mod.rs`.
///
/// `vm` is the raising VM's `SharedVm::vm_identity`. It is a parameter rather
/// than something read off `frame`/`thread` because neither `Frame` nor
/// `JvmThread` carries a VM identity — see the module note on
/// `push_frame_and_fire_entry`. Every caller has `shared: &SharedVm` in
/// scope, so the value is always exact and never the unattributed seam.
#[inline]
pub(super) fn fire_jvmti_exception_catch(vm: usize, frame: &Frame, handler_pc: usize) {
    let method_id = synth_method_id(frame);
    // Thread id is implicit in JVMTI's ExceptionCatch callback signature;
    // we pass 0 here (the interpreter does not track a JVMTI thread id on
    // the per-frame path). Agents that need the id consult `GetCurrentThread`
    // from within the callback.
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    crate::runtime::jvmti::fire_exception_catch_for_vm(vm, 0, method_id, handler_pc as i64);
}

/// Synthesize a stable JVMTI `MethodId` for `frame`.
///
/// The encoding packs the 32-bit class id in the upper 32 bits of a `u64`
/// and an FNV-1a hash of the method name in the lower 32 bits. This lets
/// both the interpreter's event fires and agents' callback arguments agree
/// on a single identifier without a real method table lookup. Descriptor
/// is intentionally NOT hashed because JVMTI agents treat overloads as
/// separate method ids only when a real jmethodID is available.
#[inline]
pub(crate) fn synth_method_id(frame: &Frame) -> u64 {
    let class_id = frame.class_id.as_u32();
    let mut h: u32 = 2166136261;
    for b in frame.method_name().bytes() {
        // Widening: smaller value -> u32 (value fits)
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    ((class_id as u64) << 32) | (h as u64)
}

// ---------------------------------------------------------------------------
// T17.Δ — interpreter-side JVMTI event fire helpers
// ---------------------------------------------------------------------------
//
// These helpers wrap the per-event fast-path check + free-function fire so
// the interpreter hot path stays compact. Every helper returns early on a
// single `AtomicBool::Acquire` load when no agent is subscribed to the
// corresponding event, adding < 2 ns per opcode in the no-agent case.
//
// **VM scoping.** Each helper takes `vm: usize` — the raising VM's
// `SharedVm::vm_identity` — as its first parameter, and delivers through the
// `runtime::jvmti::fire_*_for_vm` family, which resolves that VM's JVMTI
// environment and re-checks *that environment's* enable set before invoking a
// callback. The `any_*_listener_active()` calls immediately below are the
// deliberate process-wide **union** pre-filter: with two VMs the union can be
// true because the *other* VM has an agent, which costs this VM one predicted
// branch plus one registry lookup and never delivers it another VM's event.
// Guards may over-approximate; delivery may not.
//
// `vm` is a parameter and not a field read off `thread`/`frame` because
// neither `JvmThread` (`vm/src/threading/jvm_thread.rs:336`) nor `Frame`
// (`vm/src/runtime/frame.rs:249`) carries a VM identity, and the only
// process-level `SharedVm` registry (`set_global_shared_vm_for_hooks`,
// `vm/src/vm/vm_init.rs:3498`) is a fan-out list of *every* live VM — it can
// answer "which VMs exist", never "which VM is running this frame". Guessing
// there would be exactly the wrong-VM delivery bug this scoping exists to
// close. Every caller of every helper below has `shared: &SharedVm` in scope,
// so the identity is always exact.

/// Fire `MethodEntry` for the frame at `frames_depth - 1` (the one just
/// pushed). Costs a single Acquire load when no agent is attached.
///
/// Currently unused — `push_frame_and_fire_entry` inlines the equivalent
/// logic so that the frame push and the event are a single chokepoint.
/// Retained for callers that already hold the pushed frame.
#[allow(dead_code)]
#[inline]
pub(super) fn fire_jvmti_method_entry(vm: usize, thread: &JvmThread, frame: &Frame) {
    if !crate::runtime::jvmti::any_method_entry_listener_active() {
        return;
    }
    let method_id = synth_method_id(frame);
    crate::runtime::jvmti::fire_method_entry_for_vm(vm, thread.thread_id.0, method_id);
}

/// Fire `MethodExit` for a normal return with the given return value.
#[inline]
pub(super) fn fire_jvmti_method_exit_normal(
    vm: usize,
    thread: &JvmThread,
    frame: &Frame,
    return_value: &Option<Value>,
) {
    if !crate::runtime::jvmti::any_method_exit_listener_active() {
        return;
    }
    let method_id = synth_method_id(frame);
    let lv = to_local_value(return_value.as_ref());
    crate::runtime::jvmti::fire_method_exit_for_vm(vm, thread.thread_id.0, method_id, false, lv);
}

/// Fire `MethodExit` for an exception-unwind exit.  The return value is
/// always `LocalValue::Object(None)` because the method did not produce a
/// value.
///
/// Currently unused — `pop_and_recycle_frame_with_reason` inlines the
/// equivalent logic so the event fires exactly once per unwind.  Retained
/// so external callers (e.g. a future JIT deopt path) can invoke it
/// directly without duplicating the fast-path gate.
#[allow(dead_code)]
#[inline]
pub(super) fn fire_jvmti_method_exit_exception(vm: usize, thread: &JvmThread, frame: &Frame) {
    if !crate::runtime::jvmti::any_method_exit_listener_active() {
        return;
    }
    let method_id = synth_method_id(frame);
    crate::runtime::jvmti::fire_method_exit_for_vm(
        vm,
        thread.thread_id.0,
        method_id,
        /*was_popped_by_exception=*/ true,
        crate::runtime::jvmti::LocalValue::Object(None),
    );
}

/// Fire `FramePop` if the about-to-be-popped frame's depth matches any
/// entry in `thread.frame_pop_requests`.  The matching entry is consumed
/// so that a single `NotifyFramePop` call yields exactly one event.
#[inline]
pub(super) fn fire_jvmti_frame_pop_if_requested(
    vm: usize,
    thread: &mut JvmThread,
    was_popped_by_exception: bool,
) {
    if !crate::runtime::jvmti::any_frame_pop_listener_active() {
        return;
    }
    if thread.frame_pop_requests.is_empty() {
        return;
    }
    // Widening: smaller value -> u32 (value fits)
    let current_depth = thread.frames.len().saturating_sub(1) as u32;
    if let Some(pos) = thread
        .frame_pop_requests
        .iter()
        .position(|d| *d == current_depth)
    {
        let frame = &thread.frames[thread.frames.len() - 1];
        let method_id = synth_method_id(frame);
        let tid = thread.thread_id.0;
        thread.frame_pop_requests.swap_remove(pos);
        crate::runtime::jvmti::fire_frame_pop_for_vm(vm, tid, method_id, was_popped_by_exception);
    }
}

/// Check single-step for this thread once per dispatched instruction.
/// Cost when no agent is subscribed: a single `AtomicBool::Acquire` load
/// (the per-event flag) plus one predicted branch.  No work otherwise.
#[inline]
pub(super) fn fire_jvmti_single_step(
    vm: usize,
    thread: &JvmThread,
    frame: &Frame,
    saved_pc: usize,
) {
    if !crate::runtime::jvmti::any_single_step_listener_active() {
        return;
    }
    // Per-thread gate: if this thread has not enabled single-step the
    // event does not fire even though some other thread may have.
    if !thread
        .single_step_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let method_id = synth_method_id(frame);
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    crate::runtime::jvmti::fire_single_step_for_vm(
        vm,
        thread.thread_id.0,
        method_id,
        saved_pc as i64,
    );
}

/// Push `frame` onto the thread and fire `MethodEntry`.  The MethodEntry
/// fire is gated on `any_method_entry_listener_active` — a single
/// `AtomicBool::Acquire` load when no agent is subscribed.
///
/// This is the single chokepoint for every interpreter frame push. If a
/// push site skips it (e.g. to call `thread.frames.push` directly for
/// setup reasons), MethodEntry will NOT fire for that frame.
///
/// `vm` is `shared.vm_identity` at every one of the ten call sites. It cannot
/// be derived from `thread` or `frame` — see the VM-scoping note at the top of
/// this helper block.
#[inline]
/// Fire `MethodEntry` for the frame on top of the stack.
///
/// Split out of [`push_frame_and_fire_entry`] for the fast doors, which
/// install a callee by rebuilding a retired `FrameStack` slot in place and so
/// never hand a `Frame` to that function. Only the JVMTI event is shared: the
/// Spring trace and the bytecode dump beside it are diagnostics of the
/// by-value push and stay there.
#[inline]
pub(crate) fn fire_method_entry_after_push(vm: usize, thread: &mut JvmThread) {
    if crate::runtime::jvmti::any_method_entry_listener_active() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        let method_id = synth_method_id(frame_ref);
        let tid = thread.thread_id.0;
        crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id);
    }
}

pub(crate) fn push_frame_and_fire_entry(vm: usize, thread: &mut JvmThread, frame: Frame) {
    // A retired slot at this depth is about to be overwritten by the frame
    // just built. Harvest its buffers into the thread pools first, so the
    // pooled constructors keep the recycling they have always relied on —
    // without this, a workload whose calls do not go through a fast door
    // would free a set of buffers and allocate a fresh one every call.
    thread.harvest_retired_slot();
    thread.frames.push(frame);
    fire_entry_and_diagnostics_after_push(vm, thread);
}

/// Install a frame for a cached bytecode callee on top of `thread`'s stack.
///
/// The general dispatchers' counterpart to the fast doors'
/// `invoke_fast::push_frame_verbatim`, and the same three-way ladder:
///
/// 1. **Rebuild the retired slot** at this depth. A return retires its frame
///    in place, so the four buffers are already here and none of them has to
///    travel through the thread's pools.
/// 2. **Emplace** into the next slot when none is retired — the first call at
///    a depth. Only the three buffer handles travel; the ~220-byte `Frame`
///    is written where it belongs instead of being built and moved.
/// 3. **By value**, which is what every general dispatcher did before this and
///    what `CRATONVM_JIT_NO_FRAME_EMPLACE` restores.
///
/// The one thing the doors do that this cannot is the verbatim `(slot, tag)`
/// argument transfer: a general dispatcher has already decoded its arguments
/// to `Value`s by the time it knows which callee shape it has.
///
/// `monitor_obj` is installed *after* the frame is, because two of the three
/// paths never hold a `Frame` to set it on. That is not a reordering of the
/// monitor **enter** — every caller still does that before calling this, and
/// must, since a synchronized callee's monitor has to be held before its frame
/// can run. `trace_tag` reports the caller's depth, as it did when the trace
/// ran before the push.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_cached_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    charge_phases: bool,
) {
    use crate::runtime::interpreter::invoke_phases;
    // Only `execute_invokestatic_cached` charges the phase accounting, and
    // `now()` is a relaxed load even when the instrument is off. The other
    // four call sites should not pay for an instrument they do not feed.
    let ph_t0 = if charge_phases {
        invoke_phases::now()
    } else {
        0
    };
    let depth_before = thread.frames.len();

    if crate::runtime::env_cache::no_frame_emplace() {
        // The control arm: build the frame somewhere else and move it in.
        let mut frame = Frame::new_pooled_cached(
            cached,
            args,
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        );
        frame.monitor_on_exit = monitor_obj;
        trace_frame_push(trace_tag, depth_before, &frame);
        let ph_t1 = if charge_phases {
            invoke_phases::now()
        } else {
            0
        };
        // `push_frame_and_fire_entry` harvests the retired slot, so this arm
        // also turns slot reuse off for the general dispatchers — which is
        // exactly the state they were in before this change.
        push_frame_and_fire_entry(shared.vm_identity, thread, frame);
        site_stats::bump(site_stats::INSTALL_BYVALUE);
        charge_install(charge_phases, ph_t0, ph_t1);
        return;
    }

    if !crate::runtime::env_cache::no_frame_slot_reuse()
        && thread.frames.has_retired_slot()
        && thread
            .frames
            .push_cached_value_reusing(Arc::clone(&cached), args)
    {
        let ph_t1 = if charge_phases {
            invoke_phases::now()
        } else {
            0
        };
        install_tail(shared, thread, monitor_obj, trace_tag, depth_before);
        site_stats::bump(site_stats::INSTALL_REUSE);
        charge_install(charge_phases, ph_t0, ph_t1);
        return;
    }

    // No slot to rebuild: harvest anything retired above us so its buffers
    // reach the pools the parts build is about to draw from, then write the
    // frame straight into the slot.
    thread.harvest_retired_slot();
    let (locals, kinds, stack, eff_max_locals) = crate::runtime::frame::take_cached_value_parts(
        &cached,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    thread
        .frames
        .emplace_cached_compact(cached, locals, kinds, stack, eff_max_locals);
    let ph_t1 = if charge_phases {
        invoke_phases::now()
    } else {
        0
    };
    install_tail(shared, thread, monitor_obj, trace_tag, depth_before);
    site_stats::bump(site_stats::INSTALL_EMPLACE);
    charge_install(charge_phases, ph_t0, ph_t1);
}

/// `P_FRAME_BUILD` for the install, `P_PUSH` for the tail after it.
#[inline]
fn charge_install(charge_phases: bool, start: u64, tail_start: u64) {
    if !charge_phases {
        return;
    }
    use crate::runtime::interpreter::invoke_phases;
    invoke_phases::charge(invoke_phases::P_FRAME_BUILD, start, tail_start);
    invoke_phases::charge(invoke_phases::P_PUSH, tail_start, invoke_phases::now());
}

/// What both in-place install paths do once the frame is live: the monitor the
/// caller already entered, the frame trace, the JVMTI entry event and the
/// push-time diagnostics.
#[inline]
fn install_tail(
    shared: &SharedVm,
    thread: &mut JvmThread,
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    depth_before: usize,
) {
    if monitor_obj.is_some() {
        if let Some(top) = thread.frames.last_mut() {
            top.monitor_on_exit = monitor_obj;
        }
    }
    if trace_tag.is_some() {
        let top = thread.frames.len() - 1;
        let frame = &thread.frames[top];
        trace_frame_push(trace_tag, depth_before, frame);
    }
    fire_entry_and_diagnostics_after_push(shared.vm_identity, thread);
}

/// `CRATONVM_DBG_FRAME_TRACE=1` — one line per frame push, at the depth the
/// caller was at.
#[inline]
fn trace_frame_push(tag: Option<&str>, depth: usize, frame: &Frame) {
    let Some(tag) = tag else { return };
    if !crate::runtime::env_cache::frame_trace() {
        return;
    }
    eprintln!(
        "[FRAME_PUSH/{}] depth={} {}.{}{}",
        tag,
        depth,
        frame.class_name(),
        frame.method_name(),
        frame.method_descriptor()
    );
}

/// The JVMTI `MethodEntry` event plus every push-time diagnostic that reads
/// the frame just installed.
///
/// Split out of [`push_frame_and_fire_entry`] so that
/// [`install_cached_frame`]'s in-place paths, which never hand a `Frame` to
/// that function, still run all of them — an `SBF-TRACE` that goes dark for
/// the calls that happen to take a cheaper install is worse than no trace.
pub(crate) fn fire_entry_and_diagnostics_after_push(vm: usize, thread: &mut JvmThread) {
    fire_method_entry_after_push(vm, thread);
    if crate::runtime::env_cache::trace_sb_filter() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        let cn = frame_ref.class_name();
        let mn = frame_ref.method_name();
        if cn.contains("FilteringSpringBootCondition")
            || cn.contains("OnClassCondition")
            || cn.contains("OnBeanCondition")
            || cn.contains("OnWebApplicationCondition")
            || cn.contains("AutoConfigurationImportSelector")
            || cn.contains("AutoConfigurationImportFilter")
            || (cn.contains("SpringFactoriesLoader")
                && (mn == "loadFactories" || mn == "loadFactoryNames" || mn == "load"))
            || cn.contains("ImportCandidates")
        {
            eprintln!(
                "[SBF-TRACE] enter {}.{}{}",
                cn,
                mn,
                frame_ref.method_descriptor()
            );
        }
    }
    // TEMP DIAGNOSTIC (CRATONVM_DBG_BYTECODE_DUMP, 2026-07-15
    // JRubyScriptTemplateTests investigation, round 3): dump the raw
    // bytecode + a best-effort mnemonic disassembly for every frame whose
    // class name matches the runtime-generated JRuby snippet under
    // investigation (`uri_3a_classloader....rubygems.version`, the
    // URI-mangled class name JRuby's IR-to-bytecode compiler produces for
    // `rubygems/version.rb`'s method bodies). These snippets are
    // synthesized at runtime -- there is no static .class file `javap` can
    // decompile -- so this is the only way to see the literal opcode
    // sequence CratonVM is actually executing around the `ivarGet`/
    // `invoke:sub` invokedynamic call sites.
    if crate::runtime::env_cache::dbg_bytecode_dump() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        let cn = frame_ref.class_name();
        if cn.contains("version") {
            let mn = frame_ref.method_name();
            let md = frame_ref.method_descriptor();
            let code = &frame_ref.code;
            eprintln!("[BYTECODE-DUMP] {}.{}{} ({} bytes)", cn, mn, md, code.len());
            let mut pc = 0usize;
            while pc < code.len() {
                let op = code[pc];
                let (mnemonic, extra_len) = opcode_mnemonic_and_operand_len(op, code, pc);
                let operand_bytes: Vec<u8> =
                    code[pc + 1..(pc + 1 + extra_len).min(code.len())].to_vec();
                eprintln!(
                    "  {:4}: {:02x} {:<20} operands={:?}",
                    pc, op, mnemonic, operand_bytes
                );
                pc += 1 + extra_len;
            }
        }
    }
    // TEMP DIAGNOSTIC (CRATONVM_DBG_DUPCALL_FILTER, 2026-07-23, WFLYCTL0079
    // investigation): "An attribute named 'hornetq-store-enable-async-io'
    // is already registered at location '/subsystem=transactions'" fires
    // ~1/1600 WildFly boots from inside
    // ParallelExtensionAddHandler$ExtensionInitializeTask.call(), which
    // decompiled bytecode shows invokes `ExtensionAddHandler.
    // initializeExtension(module, ...)` exactly once per task instance —
    // each extension gets exactly one task submitted to the boot executor.
    // A single-threaded, deterministic registration bug inside WildFly
    // would fail EVERY boot, not ~1/1600, so the leading hypothesis is
    // that the SAME task object's `call()` is somehow entered twice (an
    // executor/queue double-dispatch race) rather than a WildFly-side
    // logic bug. This is directly testable: log (receiver identity,
    // thread, entry ordinal) on every entry to this one method and see if
    // the same receiver address appears twice. Filtered by exact class
    // name (not a substring) since this must be cheap enough to run a
    // multi-hundred-boot campaign with it always on.
    if crate::runtime::env_cache::dbg_dupcall_filter() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        if frame_ref.method_name() == "call"
            && frame_ref.class_name()
                == "org/jboss/as/controller/extension/ParallelExtensionAddHandler$ExtensionInitializeTask"
        {
            let recv = frame_ref.get_local(0);
            let recv_addr = match recv {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            static ORDINAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let ord = ORDINAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[DUPCALL] #{ord} ExtensionInitializeTask.call() recv=0x{recv_addr:x} tid={}",
                thread.thread_id.0,
            );
        }
        // WFLYCTL0079 round 2: the executor-dispatch trace above (each
        // ExtensionInitializeTask.call() runs exactly once, modulo the
        // Callable<V> bridge pair) never caught a genuine repeat in 1600
        // boots. The actual registry mutation is several frames deeper —
        // `ExtensionAddHandler.initializeExtension` eventually calls
        // `registerAttributes(ManagementResourceRegistration)` on the
        // transactions subsystem's root resource definition, which is
        // where `HORNETQ_STORE_ENABLE_ASYNC_IO` actually gets registered
        // (via an `AliasedHandler`, decompiled bytecode confirms). A
        // retry/re-registration at THIS level wouldn't require
        // `call()` itself to re-run. Trace (receiver, registration-arg)
        // identity pairs directly at the registration call site instead.
        if frame_ref.method_name() == "registerAttributes"
            && frame_ref.class_name()
                == "org/jboss/as/txn/subsystem/TransactionSubsystemRootResourceDefinition"
        {
            let recv = frame_ref.get_local(0);
            let recv_addr = match recv {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            let reg = frame_ref.get_local(1);
            let reg_addr = match reg {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            static ORDINAL2: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let ord = ORDINAL2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[DUPREG] #{ord} TransactionSubsystemRootResourceDefinition.registerAttributes() recv=0x{recv_addr:x} registration=0x{reg_addr:x} tid={}",
                thread.thread_id.0,
            );
        }
    }
}

/// Delivery-side VM scoping for the interpreter's JVMTI event helpers.
///
/// The registry half of this (`runtime::jvmti::ENVIRONMENTS`) is pinned by
/// `runtime::jvmti`'s own tests. What is pinned *here* is the half those
/// cannot reach: that the interpreter helpers pass a real, exact
/// `vm_identity` down to the `fire_*_for_vm` family, rather than the
/// `UNATTRIBUTED_VM` migration seam they used before this change. A
/// regression that reverts any one helper to the VM-less `fire_*` free
/// function delivers VM A's MethodEntry to VM B's `-agentpath:` agent, which
/// a debugging interface people trust to be authoritative must never do.
///
/// Parallel safety: every test takes its own `scoped_vm()` identity from a
/// base no real `SharedVm` can reach (`NEXT_VM_IDENTITY` counts from 1), and
/// takes `jvmti_registry_test_lock()` because registering a listener moves
/// the process-wide **union** mirrors that `runtime::jvmti`'s tests assert
/// are false. Each test drops its rows again on the way out.
#[cfg(test)]
mod jvmti_delivery_scoping_tests {
    use super::*;
    use crate::runtime::jvmti::{
        self, EventCallbacks, EventMode, JvmtiEventKind, JvmtiEventManager, MethodId,
    };
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use std::sync::{Arc, Mutex};

    /// A `vm_identity` no other test and no real VM can collide with. Real
    /// identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs:9`), a
    /// counter starting at 1, so small integers are NOT safe to fake with in a
    /// binary that also builds real `SharedVm`s. The base is distinct from
    /// `runtime::jvmti`'s own `scoped_test_vm()` base (`0x7000_0000`) so the
    /// two modules cannot hand out the same row even by accident.
    fn scoped_vm() -> usize {
        use std::sync::atomic::AtomicUsize;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        0x7100_0000 + NEXT.fetch_add(1, AtomicOrdering::Relaxed)
    }

    /// Install an empty manager owned by `vm` and return it.
    fn manager_for(vm: usize) -> Arc<JvmtiEventManager> {
        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
        assert_eq!(
            mgr.vm_identity(),
            vm,
            "manager_for_vm must not resolve through the unattributed seam for an installed row"
        );
        mgr
    }

    /// Subscribe `mgr`'s VM to `kinds` and install `callbacks`.
    ///
    /// `set_event_callbacks` replaces the whole struct, so it is called once
    /// with every callback the test needs — calling it per kind would silently
    /// drop all but the last, which is exactly the shape of bug that makes a
    /// scoping test pass for the wrong reason.
    fn watch(mgr: &Arc<JvmtiEventManager>, kinds: &[JvmtiEventKind], callbacks: EventCallbacks) {
        for kind in kinds {
            mgr.set_event_notification_mode(EventMode::Enable, *kind, None)
                .expect("enabling an event on a fresh manager cannot fail");
        }
        mgr.set_event_callbacks(callbacks)
            .expect("installing callbacks on a fresh manager cannot fail");
    }

    type Seen = Arc<Mutex<Vec<(u64, MethodId)>>>;

    fn seen() -> Seen {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn method_entry_cb(sink: &Seen) -> EventCallbacks {
        let s = sink.clone();
        EventCallbacks {
            method_entry: Some(Box::new(move |t, m| s.lock().unwrap().push((t, m)))),
            ..Default::default()
        }
    }

    fn method_exit_cb(sink: &Seen) -> EventCallbacks {
        let s = sink.clone();
        EventCallbacks {
            method_exit: Some(Box::new(move |t, m, _exc, _rv| {
                s.lock().unwrap().push((t, m))
            })),
            ..Default::default()
        }
    }

    fn test_frame(method_name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            method_name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    fn test_thread(name: &str) -> JvmThread {
        JvmThread::new(ThreadId(1234), name)
    }

    /// MethodExit raised with VM A's identity reaches A's agent and never B's.
    #[test]
    fn method_exit_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        watch(&ma, &[JvmtiEventKind::MethodExit], method_exit_cb(&sa));
        watch(&mb, &[JvmtiEventKind::MethodExit], method_exit_cb(&sb));

        let thread = test_thread("method-exit-scoping");
        let frame = test_frame("m");
        let expected = synth_method_id(&frame);
        fire_jvmti_method_exit_normal(a, &thread, &frame, &Some(Value::Int(7)));

        assert_eq!(
            *sa.lock().unwrap(),
            vec![(thread.thread_id.0, expected)],
            "the raising VM's agent must see exactly one MethodExit"
        );
        assert!(
            sb.lock().unwrap().is_empty(),
            "a second VM's agent must never see another VM's MethodExit"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// `push_frame_and_fire_entry` is the single frame-push chokepoint, so a
    /// missed identity there mis-attributes *every* MethodEntry in the VM.
    #[test]
    fn push_frame_and_fire_entry_attributes_method_entry_to_its_vm() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        watch(&ma, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sa));
        watch(&mb, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sb));

        let mut thread = test_thread("method-entry-scoping");
        let frame = test_frame("entered");
        let expected = synth_method_id(&frame);
        push_frame_and_fire_entry(b, &mut thread, frame);

        assert_eq!(thread.frames.len(), 1, "the frame must still be pushed");
        assert_eq!(
            *sb.lock().unwrap(),
            vec![(thread.thread_id.0, expected)],
            "the pushing VM's agent must see the MethodEntry"
        );
        assert!(
            sa.lock().unwrap().is_empty(),
            "the other VM's agent must not see it"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// FramePop consumes the request on the raising VM's thread and delivers
    /// to that VM only.
    #[test]
    fn frame_pop_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::FramePop],
            EventCallbacks {
                frame_pop: Some(Box::new(move |t, m, _exc| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::FramePop],
            EventCallbacks {
                frame_pop: Some(Box::new(move |t, m, _exc| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let mut thread = test_thread("frame-pop-scoping");
        thread.frames.push(test_frame("popping"));
        let expected = synth_method_id(&thread.frames[0]);
        thread.frame_pop_requests.push(0);
        fire_jvmti_frame_pop_if_requested(a, &mut thread, false);

        assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
        assert!(sb.lock().unwrap().is_empty());
        assert!(
            thread.frame_pop_requests.is_empty(),
            "NotifyFramePop is one-shot: the matching request must be consumed"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// SingleStep keeps its per-thread gate *and* gains the per-VM one.
    #[test]
    fn single_step_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::SingleStep],
            EventCallbacks {
                single_step: Some(Box::new(move |t, m, _loc| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::SingleStep],
            EventCallbacks {
                single_step: Some(Box::new(move |t, m, _loc| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let thread = test_thread("single-step-scoping");
        let frame = test_frame("stepped");
        let expected = synth_method_id(&frame);

        // Per-thread gate closed: nobody hears it, whatever the VM.
        fire_jvmti_single_step(a, &thread, &frame, 3);
        assert!(
            sa.lock().unwrap().is_empty(),
            "the per-thread single-step gate must still be honoured"
        );

        thread
            .single_step_enabled
            .store(true, AtomicOrdering::Relaxed);
        fire_jvmti_single_step(a, &thread, &frame, 3);
        assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
        assert!(sb.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// ExceptionCatch is the one helper that has only a `&Frame` — no thread,
    /// no VM — so it is the likeliest to be reverted to the VM-less fire.
    #[test]
    fn exception_catch_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::ExceptionCatch],
            EventCallbacks {
                exception_catch: Some(Box::new(move |t, m, _loc| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::ExceptionCatch],
            EventCallbacks {
                exception_catch: Some(Box::new(move |t, m, _loc| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let frame = test_frame("catcher");
        let expected = synth_method_id(&frame);
        fire_jvmti_exception_catch(b, &frame, 17);

        assert_eq!(
            *sb.lock().unwrap(),
            vec![(0u64, expected)],
            "ExceptionCatch carries thread id 0 by design; the VM must still be exact"
        );
        assert!(sa.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// The load-bearing asymmetry: the `any_*_listener_active()` guards are a
    /// process-wide **union**, so a VM with no agent at all still enters the
    /// helper body when some *other* VM is listening. Delivery must then find
    /// nothing. If a helper trusted the union flag instead of re-resolving the
    /// row, this is the test that fails — and the bug it would be hiding is an
    /// event delivered to an agent that never asked for it.
    #[test]
    fn a_union_guard_never_delivers_another_vms_event() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (quiet, loud) = (scoped_vm(), scoped_vm());
        let _quiet_mgr = manager_for(quiet); // installed, but subscribes to nothing
        let loud_mgr = manager_for(loud);
        let heard = seen();
        let (entry_sink, exit_sink) = (heard.clone(), heard.clone());
        watch(
            &loud_mgr,
            &[JvmtiEventKind::MethodEntry, JvmtiEventKind::MethodExit],
            EventCallbacks {
                method_entry: Some(Box::new(move |t, m| {
                    entry_sink.lock().unwrap().push((t, m))
                })),
                method_exit: Some(Box::new(move |t, m, _exc, _rv| {
                    exit_sink.lock().unwrap().push((t, m))
                })),
                ..Default::default()
            },
        );

        // The union is true because `loud` is listening — that is the whole
        // point of the guard, and it is what puts `quiet`'s interpreter on the
        // slow path.
        assert!(
            jvmti::any_method_entry_listener_active(),
            "the union guard must be set while any VM listens"
        );
        assert!(
            !jvmti::any_method_entry_listener_active_for_vm(quiet),
            "the exact per-VM query must disagree with the union here"
        );

        let mut thread = test_thread("union-guard");
        fire_jvmti_method_exit_normal(quiet, &thread, &test_frame("m"), &None);
        push_frame_and_fire_entry(quiet, &mut thread, test_frame("m"));

        assert!(
            heard.lock().unwrap().is_empty(),
            "an over-approximating guard must not turn into an over-approximating delivery"
        );

        jvmti::forget_vm_jvmti_state(quiet);
        jvmti::forget_vm_jvmti_state(loud);
    }

    /// Field watchpoints: the four getfield/getstatic/putfield/putstatic sites
    /// pass `shared.vm_identity`, so a watch armed in one VM must not fire on
    /// the same `(class_id, field_index)` in another. `class_id` is only
    /// unique *within* a VM, so this pair genuinely aliases.
    #[test]
    fn field_watchpoints_do_not_alias_across_vms() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::FieldAccess],
            EventCallbacks {
                field_access: Some(Box::new(move |t, m, _f| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::FieldAccess],
            EventCallbacks {
                field_access: Some(Box::new(move |t, m, _f| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let (class_id, field_index) = (0xDEAD_BEEF_u64, 3usize);
        jvmti::set_field_watchpoint_for_vm(a, class_id, field_index, true, false)
            .expect("arming a watchpoint on a fresh row cannot fail");

        // Exactly the call the getstatic/getfield sites now make.
        jvmti::fire_field_access_if_watched_for_vm(b, 1234, 0x99, class_id, field_index);
        assert!(
            sb.lock().unwrap().is_empty(),
            "VM B has no watch on this (class_id, field_index) — B's ids mean different classes"
        );
        assert!(
            sa.lock().unwrap().is_empty(),
            "and B's access must certainly not be reported to A, which does watch it"
        );

        jvmti::fire_field_access_if_watched_for_vm(a, 1234, 0x99, class_id, field_index);
        assert_eq!(
            sa.lock().unwrap().len(),
            1,
            "A's own access must reach A's agent"
        );
        assert!(sb.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }
}
