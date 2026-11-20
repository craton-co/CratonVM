#[allow(unused_imports)]
use super::*;


/// JIT round 18 wave 9, lane osrframe
/// (`r18-compatible-osr-inlined-native-throw-loses-the-inline-frame-FIXED-20261006.md`):
/// may the single-pass sibling tail call (`epilogue-without-ret` + `JMP`)
/// target a helper that takes the VM context?
///
/// `CRATONVM_JIT_SP_TAILCALL_KEEPS_VM_HELPER_FRAME`, default ON (a correctness
/// fix; `=0` restores the tail form). A context-taking thin helper
/// (`Preconditions.checkIndex`'s, `Thread.currentThread`'s, ...) can run Java,
/// raise, allocate and capture a stack trace. JMPing to one tears the caller's
/// frame down while the frame-chain mirror still names it: every stack walk
/// made inside the helper -- the throwable's capture, `getStackTrace`, the GC
/// root walk -- then reads the DEAD frame. Its safepoint-id slot holds
/// whatever the helper's own frame wrote there (`--compatible`'s
/// `Objects.checkIndex` printed `(Unknown Source)` for `Objects.java:365`),
/// and once the helper's prologue does not happen to rewrite the dead frame's
/// saved RBP with the same value, the walk loses every frame below it: an OSR
/// body's inlined callee and its bci went with it, and the trace printed the
/// back-edge line the interpreter frame was parked at. A context-free target
/// (an inline-machine-code intrinsic's out-of-line helper) cannot reach the VM
/// and keeps the tail form.
pub(in crate::x64) fn sp_tailcall_keeps_vm_helper_frame() -> bool {
    true
}

// gci i1/sp (2026-10-11) -- `gcg-g5jm-single-pass-forced-jitoomroot-compiled-
// callee-residue-20261002`, the producer-only variant of step 4 of
// `docs/feature-designs/jit-ir-optimizations.md
// spill-words-are-not-roots-DONE-20261015.md`.
//
// Every single-pass call arm copies its popped arguments somewhere else before
// the CALL: the dispatch arms into an args buffer that starts at
// `pre_pop_spill` (above the popped slots, so the copy never overwrites a
// source it has not read yet), the direct arms into ABI registers / outgoing
// stack words and a service copy above `args_frame_top`. The popped `Frame`
// slots `[post_pop_spill, pre_pop_spill)` are then owned by nothing: the
// cursor already stands below them (`pop_stack` reclaims a slot only when no
// remaining operand lives at or above it, so no current operand is in that
// range), and the copy just made is the call's only reader. But they still
// HOLD the argument values, and the call's own reservation (the buffer, the
// service copy) puts them below the live cursor for the whole call -- and
// they keep their value afterwards until some later push happens to land on
// them. The band scan roots every such word conservatively. h4a's `[sp-band]`
// census named exactly this word as g5jm's holder: at `sp=55`
// (`new WeakReference<>(chainHead)`) the marker's source slot `80` is left
// `unowned` below the cursor, and it still holds the marker at `sp=63`, for
// the whole `chainCatch` run (8/8 FAIL on h4a). HotSpot's frame is precise.
//
// Under the switch each such slot that held a REFERENCE argument is
// overwritten with 0 right after the last read of the sources (one
// `MOV qword [rbp - off], 0` per slot, the encoding of g1/s's deleted
// `emit_zero_spill_image_slot`). Sound by the same argument as the reclaim:
// the slot belongs to no operand, no splice local and no reservation (the
// range consists exactly of the slots the pops reclaimed); nothing after the
// staging reads it (the dispatch arms read the buffer, the IC hit marshal
// reads the buffer, the callee-deopt service reads the service copy, the
// sync-direct release reads the service copy, a frameless trap re-runs the
// caller from entry); the only deopt point that describes the pre-pop stack
// at this bci (`snapshot_pre_intrinsic_call`, `ReceiverTypeChanged`) is
// baked only by the intrinsic guards, all emitted BEFORE the pop. Primitive
// arguments are left alone (no root, no store).
//
// gci i2/sp (2026-10-12): DEFAULT ON (vi1-sp A1 (b): `oome-compiled-callee`
// 0/8 with it against the default's 4/8; A2 clean), `=0` is the kill. The two
// splice call arms (`inlining.rs` `emit_inline_direct_call` /
// `emit_inline_dispatch_call`) zero their popped sources the same way (the
// `gci-i1sp-spliced-direct` page's C1): inside a splice the range
// `[post_pop_spill, pre_pop_spill)` is again exactly what the pops reclaimed
// above the splice's reserved locals, and their only later readers are the
// dispatch buffer and the service copy.

/// `CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES` -- default ON since gci i2/sp
/// (`=0`, `false`, `off` or `no` turns it off; opt-in in gci i1): zero a
/// single-pass call's popped reference-argument source slots after staging
/// (see the section above). Read per call site at compile time, no static.
pub(in crate::x64) fn sp_zero_dead_arg_sources_enabled() -> bool {
    true
}

// ARGWIN21-9 (item 4 of `jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`):
// the single-pass twin of the optimizing tier's callee window. At a site whose
// callee owns its arguments (gcd d9/d, `sp_direct_call_args_owned_by_callee`)
// the caller keeps no service copy, but a reference LOCAL the call passes and
// never reads again -- `byte[] a = new byte[n]; a[0] = 7; return drop(a, n);`
// -- stays in its frame slot (or callee-saved register) and in the call's map
// and shadow push for the whole callee (`R21OomArgWindow local`, an
// `OutOfMemoryError` HotSpot's C1 does not throw: its maps come from LIR
// liveness). The map-liveness rule (`CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS`) only
// UNNAMES such a local, which must then stay unnamed until its next store; this
// ZEROES it, so a later map naming the word names null.
//
// After the argument marshal (`emit_stack_arg_setup`: every argument is in
// its ABI register or the outgoing block) and before the spill / push and the
// `CALL`, every reference local that bytecode liveness calls dead at the
// invoke AND at the next instruction (`dead_oop_locals_rule` with the
// every-dead form; handler live-ins are unioned in, and an owned site is
// unprotected anyway) is zeroed: its frame slot, and its callee-saved register
// when no other local shares that register. Skipped: a register shared with
// another local, a local an operand-stack entry below the arguments still
// aliases (`StackSlot::Frame` of its slot / `CalleeSaved` of its register), a
// debugger-observed compile, a site inside a splice, and a pc liveness does
// not cover. Nothing after the call reads such a local: the deopt frame states
// describe it `Undefined` (`build_frame_state_at`), and an owned site's
// sentinel leaves as the caller's frameless deopt, which re-runs it from entry.
//
// `CRATONVM_JIT_SP_OWNED_CALL_DEAD_LOCAL_CLEARS=0` restores the old code byte
// for byte. Read per call site at compile time, no static.

/// `CRATONVM_JIT_SP_OWNED_CALL_DEAD_LOCAL_CLEARS` -- default on, `=0` off. See
/// the section above.
pub(in crate::x64) fn sp_owned_call_dead_local_clears_enabled() -> bool {
    true
}

/// The source words a call site may zero after staging (see the section
/// above): each `Frame(off)` home of a REFERENCE argument with
/// `band_lo <= off`, `post_pop_spill <= off < pre_pop_spill`, ascending and
/// without duplicates (two arguments may alias one word). Empty when nothing
/// was reclaimed (`post_pop_spill >= pre_pop_spill`).
pub(in crate::x64::op_invoke) fn dead_arg_source_offsets(
    arg_slots: &[StackSlot],
    arg_oops: &[bool],
    band_lo: i32,
    post_pop_spill: i32,
    pre_pop_spill: i32,
) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    for (slot, &is_oop) in arg_slots.iter().zip(arg_oops.iter()) {
        if !is_oop {
            continue;
        }
        if let StackSlot::Frame(off) = *slot {
            if off > 0
                && off >= band_lo
                && off >= post_pop_spill
                && off < pre_pop_spill
                && !out.contains(&off)
            {
                out.push(off);
            }
        }
    }
    out.sort_unstable();
    out
}

/// The pure rule behind the sibling-tail gate's VM-helper term: a tail call
/// to a callee that takes the VM context is refused while the fix is on,
/// unless the callee is classified as never reaching the VM
/// (`helper_never_reaches_vm`, OSRFRAME18-2: only under
/// [`sp_tail_leaf_helpers_enabled`], and only for
/// `crate::direct_helpers::ThinHelperVmReach::Never`).
pub(in crate::x64) fn sibling_tail_target_admitted(
    callee_needs_ctx: bool,
    keeps_vm_helper_frame: bool,
    helper_never_reaches_vm: bool,
) -> bool {
    !(callee_needs_ctx && keeps_vm_helper_frame && !helper_never_reaches_vm)
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2 of
/// `jit-r18-osrframe-proposals.md`): `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`,
/// default OFF. A tail-position call to a context-taking thin helper the
/// per-helper classification (`DirectHelperTable::thin_helper_vm_reach`)
/// proves cannot reach the VM gets back the cheap form
/// [`sp_tailcall_keeps_vm_helper_frame`] took from every such helper:
///
/// * a helper with NO VM-reaching arm (`ThinHelperVmReach::Never`) keeps the
///   `epilogue + JMP` tail form: nothing it runs can walk the stack, so the
///   torn-down frame is never read;
/// * a helper whose fast arm alone is VM-free and whose VM publishes a leaf
///   twin (`ThinHelperVmReach::LeafTwin`) is CALLED through the twin with the
///   frame still up and none of the VM-entry protocol (the twin cannot walk,
///   collect, trap or deopt, so it owes no safepoint id, spill, republish, map
///   or sentinel check), and the answer is returned at once; the twin's
///   decline value falls through to the ordinary, unchanged CALL of the
///   helper itself, which may then reach the VM with its frame published.
///
/// Every other context-taking helper keeps its frame, exactly as before.
pub(in crate::x64) fn sp_tail_leaf_helpers_enabled() -> bool {
    false
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `jit-r19-tail7-proposals-RETIRED-20261009.md`): `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`,
/// default on. Four more leaf twins at a tail-position thin-helper call, with
/// [`sp_tail_leaf_helpers_enabled`]'s contract
/// (`DirectHelperTable::thin_helper_vm_reach_with`): `ThreadLocal.get()` and
/// `Unsafe.compareAndSet{Int,Long}` at the INSTANCE direct arm (which had no
/// twin site before: `try_emit_sp_instance_tail_leaf_twin`), and
/// `ArraysSupport.vectorizedHashCode` at the static arm (a decline outside the
/// `int` range, compared through R11; five arguments, so the twin call takes
/// Win64 stack arguments the tail JMP form never could). Independent of the
/// wave-7 switch: either one alone admits only its own rows.
pub(in crate::x64) fn sp_tail_leaf_helpers_wide_enabled() -> bool {
    true
}

/// Is `op` the `xreturn` that returns a call result of descriptor type
/// `ret_type` unchanged (`V` = `return`)? The sibling-tail and leaf-twin
/// sites' shape test (TAIL7-19-3, lane tail8: shared by the instance arm; the
/// static arm keeps its own copy of the same table).
pub(in crate::x64) fn tail_return_op_matches(ret_type: u8, op: u8) -> bool {
    matches!(
        (ret_type, op),
        (b'I' | b'Z' | b'B' | b'S' | b'C', 0xAC)
            | (b'J', 0xAD)
            | (b'F', 0xAE)
            | (b'D', 0xAF)
            | (b'L' | b'[', 0xB0)
            | (b'V', 0xB1)
    )
}

/// JIT round 19 proposal wave 7, lane tail7 (MEGA619-1 of
/// `jit-r19-mega6-proposals-RETIRED-20261009.md` = the caller half of THROWS18-6):
/// `CRATONVM_JIT_SP_LAZY_RETURN_REPUBLISH`, default OFF. Drop the post-call
/// frame-record republish (the RBP mirror and the compile-id mirror, two TLS
/// stores) after a single-pass call -- a direct call to a compiled body or a
/// thin helper, and the inline-cache (MIC / PIC way / vtable column) hit of a
/// virtual or interface call -- when the call is in RETURN-TAIL position, so
/// that no VM entry of this frame can follow it on the hot path.
///
/// # Why the walk is unchanged (self-contained; no GC-side change needed)
///
/// The mirrors are read only by a walk made at a VM entry of this thread (a
/// helper call, a poll slow path, a deopt or exception service), by the Rust
/// compiled-entry brackets that save and restore them, and never by a
/// cross-thread take-over, which walks the frozen RBP register
/// (`xt_root_scan::frozen_frame_chain`). The republish exists so that the
/// NEXT VM entry of this frame finds this frame named. Under
/// [`lazy_return_republish_rule`] there is none on the hot path: the call is
/// followed only by its oop map (no code but the shadow reload's loads), the
/// result push and the `xreturn`'s epilogue and `RET`. Every cold edge that
/// does enter the VM before the return already republishes first:
///
/// * the callee-deopt service (`emit_callee_deopt_service_check`) republishes
///   on its sentinel branch BEFORE it calls `service_callee_deopt` (pinned by
///   `the_callee_deopt_service_republishes_before_its_call`);
/// * the exception edge of an unprotected site goes to the shared sentinel
///   exit, whose only call is `jit_set_throw_bci` (a thread-local store) and
///   which then returns; a protected site (local handler, reason-9 frame) is
///   refused by the rule, and so are `J`/`D`/`F` results, whose sentinel peek
///   calls `jit_dispatch_threw`, and every self-locking body, whose return arm
///   releases a monitor.
///
/// After the `RET` the mirrors name the callee's dead frame where they would
/// have named this method's own dead frame: the caller is either a compiled
/// body, which republishes after its own call (or is itself a lazy
/// return-tail site and returns too), or a Rust compiled-entry bracket, which
/// restores the value it saved. Both dead frames lie below the caller's stack
/// pointer, so no reader can tell the two apart.
///
/// # What it does not do
///
/// A megamorphic hit inside a LOOP keeps its republish. Without the callee
/// half (MEGA5-18-2: the GC walk resolving the innermost compiled frame by
/// return address), the next dispatch's miss edge and the loop's poll are VM
/// entries, so a lazy placement there only MOVES the two stores (to the
/// backedge or before the next call); it removes none.
pub(in crate::x64) fn sp_lazy_return_republish_enabled() -> bool {
    false
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-1 of
/// `jit-r19-tail7-proposals-RETIRED-20261009.md`): `CRATONVM_JIT_SP_LAZY_RETURN_REPUBLISH_WINDOW`,
/// default on. Widen [`sp_lazy_return_republish_enabled`]'s first term from
/// "an `xreturn` follows the call" to "a short VM-free straight-line WINDOW and
/// then an `xreturn` follow it" ([`lazy_return_window_end`]): `x.m(); return
/// this;` (`pop ; aload_0 ; areturn`, every `StringBuilder.append` override),
/// `x.m(); return true;`, `return x.m() + 1;`. Implies the return-tail form
/// itself (either switch admits the plain `xreturn` case).
///
/// # Why this is the whole widening the contract allows
///
/// Lane gc7 wrote the contract (`jit-r19-gc7-proposals-RETIRED-20261009.md`, MEGA5-18-2): before
/// every VM-capable transfer out of compiled code the mirrors must name the
/// current frame. A CALL TO COMPILED CODE IS ONE: any published body can be
/// made not entrant at any time (`CompiledMethod::make_not_entrant`), and its
/// patched entry jumps to a frameless stub that calls `jit_not_entrant_entry`
/// with the TLS frame mirror untouched, so that to a stack walk the helper was
/// called from the caller's own call site (`not_entrant.rs`); a retire-cell
/// site's CALL can land in `jit_invoke_dispatch` the same way. So in a loop
/// body whose VM entries are its own calls, a republish moved "to the next
/// VM entry" lands in front of the next call -- per iteration, the same two
/// stores -- and removes nothing (the dataflow-bit design of TAIL7-19-1 buys
/// only cold-edge placements and costs a by-construction audit of every helper
/// emitter of both tiers). The only post-call paths with NO VM-capable
/// transfer before they leave the frame are the ones that reach the `RET`
/// through VM-free code, which is exactly what the window admits.
pub(in crate::x64) fn sp_lazy_return_republish_window_enabled() -> bool {
    true
}

/// TAIL7-19-1 (lane tail8): the bytecodes a lazy return-tail WINDOW may hold,
/// with their lengths. Each is lowered by `walk_local_stack`
/// (`x64/op_local_stack.rs`, which emits no call of any kind) or by the GPR
/// arms of `walk_arith` (whose only call is the `frem`/`drem` helper), and none
/// can throw. Deliberately NOT here: `ldc*` (a resolution can call), any
/// store (`istore`/`astore`/`iinc`: dead before a return, and the shadow
/// publication of a reference store is not worth reasoning about), `idiv`/
/// `irem` (the zero-divisor throw), `checkcast` (its slow arm calls), any field
/// or array access (an implicit null check's recovery throws), `wide`, and
/// every long/float/double op (a `J`/`D`/`F` call result is refused anyway).
pub(in crate::x64) const LAZY_RETURN_WINDOW_OPS: &[(u8, usize)] = &[
    (0x00, 1), // nop
    (0x01, 1), // aconst_null
    (0x02, 1), // iconst_m1
    (0x03, 1), // iconst_0
    (0x04, 1), // iconst_1
    (0x05, 1), // iconst_2
    (0x06, 1), // iconst_3
    (0x07, 1), // iconst_4
    (0x08, 1), // iconst_5
    (0x10, 2), // bipush
    (0x11, 3), // sipush
    (0x15, 2), // iload
    (0x19, 2), // aload
    (0x1a, 1), // iload_0
    (0x1b, 1), // iload_1
    (0x1c, 1), // iload_2
    (0x1d, 1), // iload_3
    (0x2a, 1), // aload_0
    (0x2b, 1), // aload_1
    (0x2c, 1), // aload_2
    (0x2d, 1), // aload_3
    (0x57, 1), // pop
    (0x59, 1), // dup
    (0x60, 1), // iadd
    (0x64, 1), // isub
    (0x68, 1), // imul
    (0x74, 1), // ineg
    (0x78, 1), // ishl
    (0x7a, 1), // ishr
    (0x7c, 1), // iushr
    (0x7e, 1), // iand
    (0x80, 1), // ior
    (0x82, 1), // ixor
    (0x91, 1), // i2b
    (0x92, 1), // i2c
    (0x93, 1), // i2s
];

/// TAIL7-19-1 (lane tail8): the longest window the rule walks. A wrapper's
/// tail is a few bytecodes; a bound keeps the rule local.
pub(in crate::x64) const LAZY_RETURN_WINDOW_MAX_OPS: usize = 6;

/// TAIL7-19-1 (lane tail8, [`sp_lazy_return_republish_window_enabled`]): the pc
/// of the `xreturn` that ends a VM-free window starting at `start` (the
/// bytecode after the call), or `None`. Every bytecode of the window and the
/// `xreturn` itself must be no branch target (another edge would reach the
/// return without this call's elision -- harmless, but the rule stays local to
/// one straight-line run, as [`lazy_return_republish_rule`] does), each must be
/// a [`LAZY_RETURN_WINDOW_OPS`] row, and there are at most
/// [`LAZY_RETURN_WINDOW_MAX_OPS`] of them. `start` itself an `xreturn` is the
/// zero-length window (the wave-7 rule).
pub(in crate::x64) fn lazy_return_window_end(
    code: &[u8],
    start: usize,
    branch_targets: &[bool],
) -> Option<usize> {
    let mut pc = start;
    for _ in 0..=LAZY_RETURN_WINDOW_MAX_OPS {
        if branch_targets.get(pc).copied().unwrap_or(true) {
            return None;
        }
        let op = *code.get(pc)?;
        if (0xac..=0xb1).contains(&op) {
            return Some(pc);
        }
        let &(_, len) = LAZY_RETURN_WINDOW_OPS.iter().find(|(o, _)| *o == op)?;
        pc += len;
    }
    None
}

/// The pure rule behind [`sp_lazy_return_republish_enabled`]: may a call whose
/// next bytecode is `next_op` drop its post-call republish?
///
/// * `next_op` an `xreturn` (`ireturn`..`return`) that is no branch target
///   (`next_is_branch_target`: another edge reaching it would arrive without
///   this call's elision, but also without having entered the VM -- refused
///   anyway, to keep the rule local to one straight-line pair);
/// * `call_protected`: the call's pc is inside a try range (a local handler
///   or a reason-9 exception frame would enter the VM on the exception edge);
/// * `self_lock_armed`: a self-locking body releases its monitor at the
///   return (a VM entry under contention);
/// * `sync_site`: a caller-held synchronized call releases after the call;
/// * `ret_type` `J`/`D`/`F`: the exception check's sentinel peek is a helper
///   call;
/// * `epilogue_calls_a_helper`: the `CRATONVM_SHADOW_WATCH` epilogue calls the
///   watchpoint disarm helper.
pub(in crate::x64) fn lazy_return_republish_rule(
    next_op: Option<u8>,
    next_is_branch_target: bool,
    call_protected: bool,
    self_lock_armed: bool,
    sync_site: bool,
    ret_type: u8,
    epilogue_calls_a_helper: bool,
) -> bool {
    matches!(next_op, Some(0xac..=0xb1))
        && !next_is_branch_target
        && !call_protected
        && !self_lock_armed
        && !sync_site
        && !matches!(ret_type, b'J' | b'D' | b'F')
        && !epilogue_calls_a_helper
}

