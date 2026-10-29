// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Not-entrant compiled bodies (interpreter round i1 wave 22, lane L6).
//!
//! `docs/feature-designs/jit-ir-optimizations.md`,
//! stages 1-3, for x86-64.
//!
//! # The problem
//!
//! A body withdrawn from the [`JitCache`] stays mapped for as long as anything
//! baked its address, and every baked caller keeps entering it: the plain
//! direct `CALL rel32` of a method-entry body outside a loop, the instance-site
//! direct binds, the optimizing tier's direct calls
//! (`docs/feature-designs/jit-ir-optimizations.md`,
//! shapes 1-3). After a class redefinition that ran the OLD bytecode on every
//! such call a running compiled caller made.
//!
//! # What HotSpot does, and what this does
//!
//! `nmethod::make_not_entrant` patches the verified entry with a jump to
//! `SharedRuntime::get_handle_wrong_method_stub()`, and the callers pay nothing
//! until they arrive there. Here:
//!
//! 1. **A patchable entry.** Both x86-64 backends open every method-entry body
//!    with [`ENTRY_PATCH_PAD`], a single five-byte `NOP`, at the buffer's start
//!    (page-aligned, so 8-aligned). No thread can be stopped inside it, and one
//!    aligned 8-byte store replaces it with a `JMP rel32` atomically for any
//!    thread about to fetch it. Cost: one NOP per call.
//! 2. **The stub** ([`not_entrant_stub_bytes`]), written at patch time into the
//!    tail every [`crate::ExecutableBuffer`] maps past its capacity
//!    ([`crate::NOT_ENTRANT_TAIL_RESERVE`]), so it is always in `rel32` reach
//!    and dies with the body. It is an i2c-style adapter in reverse, HotSpot's
//!    c2i: it spills the ABI argument registers, and calls the VM's
//!    `jit_not_entrant_entry(record, regs, caller_stack_args)` with a pointer to
//!    this body's [`NotEntrantRecord`]. The VM re-dispatches the call by name
//!    through the interpreter's invoke path (non-virtually: the caller already
//!    selected this method), which reaches the class's CURRENT bytecode or its
//!    newest body, and returns the result in the compiled ABI -- or the
//!    `i64::MIN` sentinel with the pending-exception signal, exactly as a
//!    compiled callee that threw. The stub builds no frame and leaves RBP and
//!    the published frame identity alone, so to every stack walker the helper
//!    was called from the caller's own call site, whose map is published.
//!    Unlike the proposal's first sketch (return a "refused" sentinel for the
//!    caller's callee-deopt service to re-dispatch) this needs nothing from the
//!    caller: every caller shape -- serviced or not, either tier, Rust, the
//!    interpreter -- gets the right answer.
//! 3. **When.** At a redefinition, for the bodies it makes stale by record
//!    (they declare the class, inlined from it, or carry its label), published
//!    or only reachable through a baked caller's roots, after the eviction
//!    withdrew them ([`JitCache::make_not_entrant`], called by the VM's
//!    `JitRealm::redefine_and_flush`).
//!
//! A frame already running a patched body keeps running its old code, which is
//! JVMS 5.4.3 / JEP 109's obsolete-method semantics; only NEW entries are
//! redirected. A splice (shape 4) and a direct self-call that jumps past the
//! entry are not entries: a frame running a splice of a redefined callee is
//! sent to the interpreter at its next back-edge poll instead (wave 23, lane
//! L6: [`CompiledMethod::is_withdrawn_by_redefinition`], read by the VM's
//! `polling_body_must_leave`).
//!
//! # Forwarding (wave 23, lane L6)
//!
//! HotSpot's wrong-method stub re-resolves the caller's call site, so a running
//! loop pays the re-dispatch once. Wave 22's stub re-dispatched through the VM
//! on EVERY call, because nothing re-binds the caller. Rather than patching the
//! caller's `CALL rel32` (its displacement is not 4-aligned, the caller's
//! mapping would need its own W^X window, and a Rust door or a stale inline
//! cache has no call instruction to patch at all), the stub itself learns the
//! current body: its first instructions read the record's forward word
//! ([`NotEntrantRecord::forward_to`], filled by the VM's helper from the
//! cache's published body for the same key) and, while that body is neither
//! retired nor superseded, `JMP` straight to its entry, with the caller's
//! arguments and return address untouched -- the callee sees exactly the call
//! a freshly compiled caller would have baked. Cost after the first slow call:
//! the patched `JMP`, a load, two flag tests (one when the two flags are
//! adjacent bytes, wave 25) and an indirect `JMP`.
//!
//! # Forced exit polls (wave 25, lane L6)
//!
//! A frame already running a withdrawn body leaves at a back-edge poll's slow
//! path, which a poll takes only while a pause is requested; a thread inside a
//! call during the redefinition's handshake (blocked, or the redefining thread
//! itself) passed none and kept running the old splice after the call
//! returned. [`JitCache::force_withdrawn_exit_polls`] rewrites the
//! flag-clear branch of every exit-capable poll of such a body
//! ([`CompiledMethod::exit_poll_sites`], recorded by both x86-64 tiers) so it
//! always asks the slow path: every frame running the body leaves at its next
//! exit-capable back edge, on whichever thread and whenever its call returns.
//! No new activation reaches those polls (the entry is not entrant, the OSR
//! door refuses the body), so nothing else pays for them.
//!
//! # Post-call exits (wave 27, lane L6)
//!
//! A forced poll is reached only at the body's next back edge, so the code
//! between the call's return and that back edge still ran the old splice
//! (`tools/probes/interp/L6/RedefineSpliceAfterTheCallProbe.java`). HotSpot
//! patches the frame's return address to the deopt blob and resumes the
//! interpreter at the invoke's successor. Here the single-pass tier's OSR
//! compiles put a five-byte `NOP` ([`ENTRY_PATCH_PAD`]) after each real call
//! whose successor state an `OsrExit` map can describe
//! ([`CompiledMethod::post_call_exit_sites`]): a `REEXECUTE` point at the
//! SUCCESSOR bci with the call's result on the stack is exactly the state
//! "resume after the invoke", and the sinks take it as they take a
//! conditional back edge's map. The same pass that forces the polls rewrites
//! each such `NOP` into a `JMP rel32` to the site's out-of-line stub, which
//! saves every register, asks the safepoint slow path for this body's verdict
//! without parking ([`crate::POST_CALL_EXIT_VERDICT_ONLY`]), restores them and
//! leaves through the map on a non-zero verdict. The flag-clear fast path is
//! the `NOP` and nothing else.
//!
//! The write is one aligned 8-byte store: the emitter places each site so
//! that its five bytes lie inside one 8-aligned word (padding with a shorter
//! `NOP` first when they would not), and the store rewrites the word's other
//! bytes with their current values, read under the same mutation lock. A
//! thread fetching the site -- including one whose call returns there at
//! that moment -- sees the whole `NOP` or the whole `JMP`, both correct.
//!
//! # Supersede redirect (nio21 wave 16, lane L1)
//!
//! A publication that displaces a body of the same key makes the displaced
//! body not entrant, its stub forwarding to the current body
//! ([`JitCache::redirect_superseded_locked`], from [`JitCache::put`];
//! `CRATONVM_JIT_SUPERSEDE_REDIRECT=0` is the kill switch). hm5 did this only
//! from the VM's C2-publish path, for the body the C2 task READ before it
//! compiled, and only for C1 -> C2; a single-pass body displaced by a second
//! single-pass compile kept every caller that baked it -- and their calls,
//! which its own entry counter took and the drain never read -- for life
//! (`TreeMap.put(K,V,Z)` / `addEntry` / `fixAfterInsertion` on `tmapcmp`).
//! An optimizing body displaced by a single-pass one is left as it is.

use crate::{CompiledMethod, JitCache, JitInvokeInfo};
use std::sync::Arc;

/// The first instruction of every x86-64 method-entry body the single-pass
/// driver and the optimizing tier emit: `NOP DWORD PTR [RAX+RAX*1+0]`, one
/// instruction of five bytes, so the not-entrant patch can replace it with a
/// `JMP rel32` in one aligned 8-byte store without any thread being able to
/// sit between its bytes.
pub const ENTRY_PATCH_PAD: [u8; 5] = [0x0F, 0x1F, 0x44, 0x00, 0x00];

/// Upper bound on [`not_entrant_stub_bytes`]'s length: the 41-byte forwarding
/// prefix plus 64 bytes on Win64 (105) or 71 on SysV (112). The stub starts at
/// the first 16-byte boundary at or after the emitted code (at most 15 bytes
/// past it), so `15 + 112 = 127` fits the [`crate::NOT_ENTRANT_TAIL_RESERVE`]
/// (128) every buffer maps past its capacity, even for a full buffer.
pub const NOT_ENTRANT_STUB_MAX: usize = 112;

/// Kill switch for the stub's forwarding fast path (interpreter round i1 wave
/// 23, lane L6; module header, "Forwarding"). `false` emits wave 22's stub,
/// which re-dispatches every call through the VM.
pub const NOT_ENTRANT_FORWARDING_ENABLED: bool = true;

/// How many forwards one record may install over its life. Every forward it
/// ever published is kept (a thread may have read the word a moment ago and be
/// about to jump), so a method whose current body keeps being withdrawn would
/// otherwise keep one retired body mapped per withdrawal; past the cap the
/// record stays on the VM path, which is always correct.
pub const NOT_ENTRANT_MAX_FORWARDS: usize = 8;

/// Bodies the forward's cycle check visits at most before it gives up and
/// refuses the forward.
const FORWARD_REACH_LIMIT: usize = 4096;

/// Integer argument registers of the compiled-code ABI, the ones the stub
/// spills: RCX RDX R8 R9 on Win64, RDI RSI RDX RCX R8 R9 on SysV.
#[cfg(target_os = "windows")]
pub const NOT_ENTRANT_ARG_REGS: usize = 4;
/// See the Win64 arm.
#[cfg(not(target_os = "windows"))]
pub const NOT_ENTRANT_ARG_REGS: usize = 6;

/// What a not-entrant stub hands the VM: the method the patched body
/// implements, as a statically bound call site would name it, and how its
/// arguments arrived.
pub struct NotEntrantRecord {
    /// A synthetic call-site record for the body's own method: `invoke_kind`
    /// 3 (static) or 1 (a non-virtual instance call of exactly this method --
    /// the caller already selected it), its class, name and descriptor, and
    /// the class id as both the resolving context and the substituted owner,
    /// so the by-name re-dispatch resolves this loader's class. Its strings
    /// point into `_names`.
    pub info: JitInvokeInfo,
    /// The `SharedVm` the body belongs to (a body that takes no context
    /// pointer has no other way to name its VM).
    pub vm_ptr: usize,
    /// Whether the body takes the VM context pointer in the first argument
    /// register, before its Java arguments.
    pub needs_context: bool,
    /// Java arguments, receiver included (the compact ABI: one per value).
    pub num_java_args: usize,
    /// Owns the text `info`'s `&'static str`s point into.
    _names: [Arc<str>; 3],
    /// The patched body's `artifact_id` and entry, stamped when its stub is
    /// installed ([`CompiledMethod::install_not_entrant_stub`]); `0` before.
    body_artifact_id: u64,
    body_entry: usize,
    /// The word the stub's forwarding prefix reads: `0`, or the address of
    /// the [`NotEntrantForward`] it jumps through (always one of `forwards`).
    /// Written with `Release` after the forward is fully built; x86-64 loads
    /// are acquire, so the stub never sees a half-built forward.
    forward: std::sync::atomic::AtomicUsize,
    /// Every forward this record ever published, never dropped before the
    /// record: a thread may have loaded the forward word just before it was
    /// replaced and still be about to jump through the old one. Bounded by
    /// [`NOT_ENTRANT_MAX_FORWARDS`].
    forwards: parking_lot::Mutex<Vec<Box<NotEntrantForward>>>,
    /// The `artifact_id` of the last target [`Self::forward_to`] refused for
    /// reaching this body (a cycle), so the next slow call does not walk the
    /// same graph again; `0` for none (artifact ids start above it).
    refused_target: std::sync::atomic::AtomicU64,
    /// The `artifact_id` of the last target whose cycle walk passed
    /// [`FORWARD_REACH_LIMIT`] (strings round, `str16`): refused by both
    /// forms, so the next slow call does not walk that graph again. Kept
    /// apart from `refused_target`, which the releasable form ignores.
    refused_too_large: std::sync::atomic::AtomicU64,
    /// HANDLER11-5 (JIT round 19 wave 12, lane handler12): the patched body
    /// was bound by a caller-held synchronized site
    /// ([`CompiledMethod::is_bound_by_a_caller_held_site`]) when its stub was
    /// built, so a forward must keep that site's contract
    /// ([`caller_held_forward_refusal`]).
    caller_held_bound: bool,
    /// Calls the VM re-dispatched for this record, counted only while
    /// `CRATONVM_DBG_DEOPT` is on ([`Self::note_redispatch`]; round 16 wave
    /// 16, lane chains2, TF15-2), so the default path never touches it.
    redispatches: std::sync::atomic::AtomicU64,
    /// STASH20-2 (JIT round 20 wave 19, lane yStash): call sites of compiled
    /// callers re-pointed at a target one of this record's forwards owns
    /// ([`JitCache::rebind_stale_call_site`]). Non-zero, no forward of this
    /// record may close a reference cycle any more
    /// ([`Self::forward_to_releasable`] refuses it): a cycle forward is the
    /// only kind the cache ever releases, and a rebound site relies on its
    /// target staying owned for as long as the patched body lives. Written
    /// and read under the cache's `mutation` lock.
    rebinds: std::sync::atomic::AtomicU32,
    /// Test only (JIT round 21 wave 13, lane trace; review w13 item 4): how
    /// many times `install_forward` moved the process-wide generation for
    /// THIS record. The generation itself is shared with every concurrent
    /// test, so only a record-local count can show the bump was made.
    #[cfg(test)]
    first_forward_bumps: std::sync::atomic::AtomicU32,
}

/// What a not-entrant stub's forwarding prefix jumps through (wave 23, lane
/// L6): the current body's entry and the addresses of its `retired` and
/// `superseded` flags, which the stub tests before the jump, at the offsets
/// the stub bakes (0, 8, 16: `repr(C)`, 8-byte words on x86-64). Owns the
/// target, so the code it names stays mapped for as long as the record (and
/// so the patched body the record belongs to) lives -- or, for a forward
/// that closes a reference cycle (strings round, `str16`), until the cache
/// withdraws the target ([`NotEntrantRecord::release_forwards_to`]), which
/// takes the owner out while the box itself stays for a thread that read the
/// word a moment ago (the target's flags then read retired, and the owner
/// leaves through the retirement queue, so its memory outlives that thread).
#[repr(C)]
pub struct NotEntrantForward {
    entry: usize,
    retired_flag: usize,
    superseded_flag: usize,
    target: parking_lot::Mutex<Option<crate::RetainedCode>>,
    /// The target's entry, which the cache's cycle-forward registry is keyed
    /// by; kept beside the owner so a released forward still names it.
    target_entry: usize,
    /// STASH20-2: the forward closes a reference cycle
    /// ([`ForwardOutcome::InstalledCycle`]), so the cache may release its
    /// owner. Past the stub's baked offsets (it reads 0, 8 and 16 only).
    cycle: bool,
}

/// What [`NotEntrantRecord::forward_to_releasable`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// No forward: the stub keeps re-dispatching through the VM.
    Refused,
    /// The stub forwards to the target, owning it for the record's life.
    Installed,
    /// The stub forwards to a target that reaches the patched body back (a
    /// reference cycle). The caller must register it with the cache, which
    /// releases the owner when the target is withdrawn
    /// ([`crate::JitCache::forward_not_entrant`]).
    InstalledCycle,
}

/// Whether [`reaches_artifact`]'s walk found the artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    No,
    Yes,
    /// More bodies than [`FORWARD_REACH_LIMIT`]: unknown, and it stays so.
    TooLarge,
    /// Another record's forward list was busy: unknown this time only.
    Busy,
}

/// hm5 (`r16-alloc-hashmap-put-get-premium-is-the-call-path-not-allocation`;
/// W19-3 of `jit-r11-lhm-proposals` = C12-1 of `jit-r12-calls-proposals`): may
/// a C1 body `old`, superseded by the C2 body `new` published under the same
/// key, be made not entrant with its stub FORWARDING to `new`?
///
/// The static half of [`NotEntrantRecord::admits_forward_target`], asked of
/// the two bodies before the patch: same ABI (context pointer, argument
/// count), `new` neither wrapped, nor holding an `invokedynamic` trap, nor
/// tagged for the inline cache's context call. A patched `old` whose stub
/// could never forward would send every baked call through the VM's by-name
/// re-dispatch, which is slower than the C1 body it replaces, so such a pair
/// is left alone. The dynamic half (retired, superseded, a reference cycle,
/// the forward cap) is still asked by `forward_to` at the first slow call.
pub fn supersede_forward_admissible(old: &CompiledMethod, new: &CompiledMethod) -> bool {
    supersede_forward_refusal(old, new).is_none()
}

/// JIT r25 wave 11 (W11-2): [`supersede_forward_refusal`]'s answer for a
/// superseded body already patched -- by the publication's own supersede
/// redirect (`JitCache::redirect_superseded_locked`), the usual case. Its stub
/// forwards to the method's current body from its first call, so nothing is
/// left entrant.
pub const ALREADY_NOT_ENTRANT: &str =
    "the C1 body is already not entrant (redirected when the C2 body was published)";

/// JIT r25 wave 10 (W10-2b): the first conjunct of
/// [`supersede_forward_admissible`] that refuses, named, or `None` when the
/// forward is admissible -- the same tests in the same order, so the
/// supersede's `CRATONVM_DBG_JITC` line can say WHY a C1 body stayed entrant
/// (its baked callers then keep running it: the steady-state `[c1]` symptom).
pub fn supersede_forward_refusal(
    old: &CompiledMethod,
    new: &CompiledMethod,
) -> Option<&'static str> {
    // Cast: addresses, compared and tested only.
    let entry = new.entry_ptr() as usize;
    if entry == 0 || entry == old.entry_ptr() as usize || new.artifact_id == old.artifact_id {
        return Some("same or no entry");
    }
    if (entry as u64) & crate::JIT_IC_NEEDS_CONTEXT_TAG != 0 {
        return Some("entry tagged for the inline cache's context call");
    }
    if new.needs_context != old.needs_context {
        return Some(if old.needs_context {
            "context ABI: the C1 body takes the VM context, the C2 body does not"
        } else {
            "context ABI: the C2 body takes the VM context, the C1 body does not"
        });
    }
    if new.entry_java_args == u16::MAX || new.entry_java_args != old.entry_java_args {
        return Some("argument count");
    }
    if new.requires_wrapped_entry {
        return Some("the C2 body needs a wrapped entry");
    }
    if new.has_indy_trap {
        return Some("the C2 body holds an invokedynamic trap");
    }
    if old.is_not_entrant() {
        // JIT r25 wave 11 (W11-2): not a lost forward. `JitCache::put`'s
        // supersede redirect patched it when the C2 body was published (it
        // asks this same admissibility), and its stub installs the forward
        // to the current body at its first call (`jit_not_entrant_entry`).
        return Some(ALREADY_NOT_ENTRANT);
    }
    caller_held_forward_refusal(old.is_bound_by_a_caller_held_site(), new)
}

/// `CRATONVM_JIT_FORWARD_KEEPS_CALLER_HELD_CLOSED` (HANDLER11-5, JIT round 19
/// wave 12, lane handler12; **default ON**, correctness, `=0` is the kill
/// word): see [`caller_held_forward_refusal`]. Read per forward decision
/// (a supersede, a not-entrant stub's first slow call), both cold.
fn forward_keeps_caller_held_closed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_FORWARD_KEEPS_CALLER_HELD_CLOSED")
}

/// JIT round 21 wave 10 (lane splice;
/// `docs/feature-designs/jit-ir-optimizations.md` item 2):
/// does a record's FIRST forward box move the process-wide JIT cache
/// generation? Rides on `CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST` (default
/// ON, `=0` is the kill word): the bump exists only because that switch's
/// trace fallback answers through [`NotEntrantRecord::ever_forwarded`], a bit
/// the raw-walk memo (`vm/src/jit/conservative_roots.rs`, `RawWalkKey`) keys
/// only through the generation. Read once per forward install that reaches
/// the box push (cold, at most [`NOT_ENTRANT_MAX_FORWARDS`] per record).
///
/// Why the bump is exact, with the memo reading the generation BEFORE its
/// walk: it is made under the record's `forwards` lock before the box is
/// pushed and the forward word stored. A walk that read `ever_forwarded ==
/// false` took that lock (`try_lock`) before the installer did, so its
/// earlier generation read cannot see the bump; a thread whose stack holds
/// the forward target's frame went through the stub, which loaded the word
/// stored after the bump, so its replay's generation read sees it.
fn first_forward_bumps_generation_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST")
}

/// HANDLER11-5 (`r19-supersede-forward-ignores-a-caller-held-binding-20261007-FIXED-20261009.md`):
/// why a body a caller-held synchronized site bound (`old_bound`) may not
/// forward to `target`, or `None`.
///
/// Such a site holds the method's monitor itself around a raw `CALL`, and the
/// VM handed it the body only after proving it closed (it stops only by a
/// return or an exception sentinel) and marking it
/// ([`CompiledMethod::mark_bound_by_a_caller_held_site`]), which keeps the
/// safepoint verdict from ever granting it a named-body exit. A forward to a
/// body nobody bound that way hands the site code that was never checked for
/// closedness and is not marked: a C2 body with guards and grantable poll
/// exits, or a self-locking body. Refused, the superseded body stays entrant
/// for the site's `CALL` ([`supersede_forward_admissible`]), or the stub keeps
/// re-dispatching through the VM, whose invoke path takes the monitor and the
/// trap service for itself; both are correct.
fn caller_held_forward_refusal(old_bound: bool, target: &CompiledMethod) -> Option<&'static str> {
    (old_bound
        && !target.is_bound_by_a_caller_held_site()
        && forward_keeps_caller_held_closed_enabled())
    .then_some("the patched body is a caller-held site's; target not bound by one")
}

impl NotEntrantRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        class_name: &Arc<str>,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        class_id: u32,
        is_static: bool,
        vm_ptr: usize,
        needs_context: bool,
        num_java_args: usize,
    ) -> Self {
        let names = [
            Arc::clone(class_name),
            Arc::clone(method_name),
            Arc::clone(descriptor),
        ];
        // SAFETY: each `str` lives in an `Arc` allocation this record owns in
        // `_names` for its whole life and never mutates, so the text outlives
        // every read through `info`; the lifetime is widened only because
        // `JitInvokeInfo` spells borrowed JIT metadata that way.
        let text = |a: &Arc<str>| -> &'static str { unsafe { &*Arc::as_ptr(a) } };
        let return_type = cratonvm_jit_api::descriptor_param_list_end(descriptor)
            .and_then(|close| descriptor.as_bytes().get(close + 1).copied())
            .unwrap_or(b'V');
        let info = JitInvokeInfo {
            class_name: text(&names[0]),
            method_name: text(&names[1]),
            descriptor: text(&names[2]),
            num_jit_args: num_java_args,
            return_type,
            invoke_kind: if is_static { 3 } else { 1 },
            declaring_class_id: class_id,
            owner_class_id: if is_static { 0 } else { class_id },
        };
        Self {
            info,
            vm_ptr,
            needs_context,
            num_java_args,
            _names: names,
            body_artifact_id: 0,
            body_entry: 0,
            forward: std::sync::atomic::AtomicUsize::new(0),
            forwards: parking_lot::Mutex::new(Vec::new()),
            refused_target: std::sync::atomic::AtomicU64::new(0),
            refused_too_large: std::sync::atomic::AtomicU64::new(0),
            caller_held_bound: false,
            redispatches: std::sync::atomic::AtomicU64::new(0),
            rebinds: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            first_forward_bumps: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Count one call the VM re-dispatched for this record and say whether
    /// the `CRATONVM_DBG_DEOPT` line should print for it: the first call and
    /// every power of two after it (round 16 wave 16, lane chains2, TF15-2 of
    /// `jit-r16-trapfix-proposals-RETIRED-20261006.md`). A not-compilable method called from a
    /// running compiled loop re-dispatches once per call; one line per call
    /// buried the signal under hundreds of thousands of lines. Returns the
    /// running count (`n=` on the line) when the line is due.
    pub fn note_redispatch(&self) -> Option<u64> {
        let n = self
            .redispatches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        n.is_power_of_two().then_some(n)
    }

    /// The calls counted by [`Self::note_redispatch`] so far.
    pub fn redispatch_count(&self) -> u64 {
        self.redispatches.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// JIT round 21 wave 8 (lane splice): whether this stub has ever
    /// published a forward (the word is set, or a kept forward exists). A
    /// busy list answers `true` (fail closed).
    /// See [`crate::CompiledMethod::not_entrant_stub_ever_forwarded`].
    pub fn ever_forwarded(&self) -> bool {
        if self.forward.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return true;
        }
        self.forwards.try_lock().is_none_or(|kept| !kept.is_empty())
    }

    /// The return address of the call the stub intercepted: the word the
    /// caller's `CALL` pushed, just below the caller's stack-passed arguments
    /// (and its 32-byte shadow space on Win64). For diagnostics.
    ///
    /// # Safety
    ///
    /// `stack_args` must be the pointer a not-entrant stub passed.
    pub unsafe fn caller_return_address(stack_args: *const i64) -> usize {
        #[cfg(target_os = "windows")]
        const BELOW: usize = 5;
        #[cfg(not(target_os = "windows"))]
        const BELOW: usize = 1;
        // SAFETY: the stub's frame holds the return address exactly `BELOW`
        // words below the stack-argument pointer it passes (see
        // `not_entrant_stub_bytes`).
        // Cast: an address word, read as one.
        unsafe { stack_args.sub(BELOW).read() as usize }
    }

    /// Forward later calls into the patched body straight to `target`, the
    /// body now published under the same key (interpreter round i1 wave 23,
    /// lane L6; module header, "Forwarding"). Returns whether the stub now
    /// forwards to `target`.
    ///
    /// Refused -- the stub keeps re-dispatching through the VM, which is
    /// always correct -- unless `target` could have been baked by a direct
    /// call of this record's callers: a method-entry body published under this
    /// record's exact key, with its ABI (context pointer, argument count),
    /// neither retired, superseded nor itself not entrant, not a body that
    /// needs the VM's synchronized wrapper or holds an `invokedynamic` trap
    /// (the cycle cell's admission, `JitCycleEdgeCell::admits`). Also refused
    /// when `target` reaches the patched body through baked callee roots or
    /// forwards: the forward would close a cycle of strong references and
    /// keep every body on it mapped forever. And once
    /// [`NOT_ENTRANT_MAX_FORWARDS`] forwards were installed.
    pub fn forward_to(&self, target: &Arc<CompiledMethod>) -> bool {
        self.install_forward(target, false) == ForwardOutcome::Installed
    }

    /// [`Self::forward_to`], except that a target reaching the patched body
    /// back is still forwarded to, as [`ForwardOutcome::InstalledCycle`]
    /// (strings round, `str16`; `docs/feature-designs/jit-ir-optimizations.md
    /// r16-strings-stringbuilder-tostring-is-a-microsecond-native-call-FIXED-20261006.md`).
    ///
    /// Such a target is common, not exotic: JDK 25 `Arrays.copyOfRange`'s
    /// throw arm builds its message with `StringBuilder`, so its new body
    /// reaches `StringBuilder.toString` -> `String.<init>(AbstractStringBuilder,
    /// Void)` -> the stale `copyOfRange` body that `String.<init>` baked.
    /// Refusing kept every `toString()` of every compiled caller on the VM's
    /// by-name re-dispatch for the life of the process (2-19 us per call
    /// against ~100 ns forwarded) in whichever process the tier-up order
    /// produced that shape -- `R16Bench strings` was bimodal per process. The
    /// cycle is the same one [`crate::JitCycleEdgeCell`] closes for mutual
    /// recursion, and it is broken the same way: only through the cache's
    /// registry ([`crate::JitCache::forward_not_entrant`]), which releases
    /// the owner when the target is withdrawn.
    ///
    /// A walk past [`FORWARD_REACH_LIMIT`] is still refused and remembered; a
    /// walk that met a busy forward list is refused for this call only (it
    /// used to be remembered as a cycle for good).
    pub(crate) fn forward_to_releasable(&self, target: &Arc<CompiledMethod>) -> ForwardOutcome {
        self.install_forward(target, true)
    }

    fn install_forward(&self, target: &Arc<CompiledMethod>, allow_cycle: bool) -> ForwardOutcome {
        use std::sync::atomic::Ordering;
        if !NOT_ENTRANT_FORWARDING_ENABLED || self.body_entry == 0 {
            return ForwardOutcome::Refused;
        }
        if !self.admits_forward_target(target) {
            return ForwardOutcome::Refused;
        }
        // Cast: an address, compared and baked only.
        let entry = target.entry_ptr() as usize;
        {
            let current = self.forward.load(Ordering::Acquire);
            if current != 0 {
                // SAFETY: a published forward word names one of `forwards`'
                // boxes, which live as long as `self`.
                let cur = unsafe { &*(current as *const NotEntrantForward) };
                if cur.entry == entry
                    && cur
                        .target
                        .lock()
                        .as_ref()
                        .is_some_and(|t| Arc::ptr_eq(t.arc(), target))
                {
                    return ForwardOutcome::Installed;
                }
            }
        }
        // STASH20-2: a forward word a call-site rebind cleared
        // ([`Self::clear_forward_word_for_rebinds`]) is put back on the box it
        // named while that box still owns `target`, instead of a new box per
        // stale caller (the cap below would end forwarding after eight).
        // Only that clear leaves an owning box unnamed, so nothing else
        // reaches this.
        if let Some(outcome) = self.reuse_kept_forward(target, entry, allow_cycle) {
            return outcome;
        }
        // Every refusal below is asked again on the next slow call, which is
        // every call while there is no forward: keep the walk off that path.
        if self.forwards.lock().len() >= NOT_ENTRANT_MAX_FORWARDS
            || self.refused_too_large.load(Ordering::Relaxed) == target.artifact_id
            || (!allow_cycle && self.refused_target.load(Ordering::Relaxed) == target.artifact_id)
        {
            return ForwardOutcome::Refused;
        }
        // Before taking this record's lock: the walk try-locks other
        // records' lists, and must never wait on one while holding its own.
        let cycle = match reaches_artifact(target, self.body_artifact_id) {
            Reach::No => false,
            // STASH20-2: never once a call site was rebound through this
            // record (`rebinds`): a cycle forward's owner is releasable.
            Reach::Yes if allow_cycle && self.rebinds.load(Ordering::Relaxed) == 0 => true,
            Reach::Busy => return ForwardOutcome::Refused,
            Reach::TooLarge => {
                self.refused_too_large
                    .store(target.artifact_id, Ordering::Relaxed);
                return ForwardOutcome::Refused;
            }
            Reach::Yes => {
                self.refused_target
                    .store(target.artifact_id, Ordering::Relaxed);
                return ForwardOutcome::Refused;
            }
        };
        let first_forward_bumps = first_forward_bumps_generation_enabled();
        let mut kept = self.forwards.lock();
        if kept.len() >= NOT_ENTRANT_MAX_FORWARDS {
            return ForwardOutcome::Refused;
        }
        // JIT round 21 wave 10 (lane splice): the first box flips
        // `ever_forwarded` false -> true for good, and the trace walk's
        // not-entrant innermost fallback answers through that bit, which
        // the raw-walk memo does not key. Move the process-wide generation
        // here, under this lock and before the box and the word are
        // published (see `first_forward_bumps_generation_enabled`).
        if first_forward_bumps && kept.is_empty() {
            crate::JIT_CACHE_GENERATION.fetch_add(1, Ordering::Release);
            #[cfg(test)]
            self.first_forward_bumps.fetch_add(1, Ordering::Relaxed);
        }
        let forward = Box::new(NotEntrantForward {
            entry,
            // Cast: the flags' addresses, baked for the stub's byte tests. An
            // `AtomicBool` has `bool`'s in-memory representation.
            retired_flag: &target.retired as *const std::sync::atomic::AtomicBool as usize,
            // Cast: as above.
            superseded_flag: &target.superseded as *const std::sync::atomic::AtomicBool as usize,
            target: parking_lot::Mutex::new(Some(crate::RetainedCode::new(Arc::clone(target)))),
            target_entry: entry,
            cycle,
        });
        // Cast: the forward's address, the word the stub reads.
        let word = &*forward as *const NotEntrantForward as usize;
        kept.push(forward);
        self.forward.store(word, Ordering::Release);
        if cycle {
            ForwardOutcome::InstalledCycle
        } else {
            ForwardOutcome::Installed
        }
    }

    /// Release every forward of this record whose target's entry is
    /// `target_entry` (the cache withdrew that body): the forward word goes
    /// back to `0` if it names one of them (the stub re-dispatches, and its
    /// next slow call may forward to the method's newer body), and each such
    /// forward's owner is taken out and returned, for the caller to drop
    /// through the retirement queue. The boxes stay: a thread may have loaded
    /// the word just before and still be testing the target's flags, which
    /// read retired or superseded by now (the cache marks a body before it
    /// calls this).
    pub(crate) fn release_forwards_to(&self, target_entry: usize) -> Vec<crate::RetainedCode> {
        use std::sync::atomic::Ordering;
        let kept = self.forwards.lock();
        let mut released = Vec::new();
        for forward in kept.iter() {
            if forward.target_entry != target_entry {
                continue;
            }
            // Cast: the forward's address, compared with the stub's word.
            let word = &**forward as *const NotEntrantForward as usize;
            if self.forward.load(Ordering::Acquire) == word {
                self.forward.store(0, Ordering::Release);
            }
            if let Some(owner) = forward.target.lock().take() {
                released.push(owner);
            }
        }
        released
    }

    /// The patched body's entry and artifact id, as the cache's cycle-forward
    /// registry names the record ([`crate::JitCache::forward_not_entrant`]).
    pub(crate) fn patched_body(&self) -> (usize, u64) {
        (self.body_entry, self.body_artifact_id)
    }

    /// STASH20-2: publish again the kept forward that owns `target` (entry
    /// `entry`) when the forward word names none, and say what it is. `None`
    /// when no kept box owns `target`, when the word already names a forward,
    /// or when the box closes a cycle and the bare form (`!allow_cycle`)
    /// asks: the ordinary path answers those.
    fn reuse_kept_forward(
        &self,
        target: &Arc<CompiledMethod>,
        entry: usize,
        allow_cycle: bool,
    ) -> Option<ForwardOutcome> {
        use std::sync::atomic::Ordering;
        if self.forward.load(Ordering::Acquire) != 0 {
            return None;
        }
        let kept = self.forwards.lock();
        let found = kept.iter().find(|f| {
            f.target_entry == entry
                && (allow_cycle || !f.cycle)
                && f.target
                    .lock()
                    .as_ref()
                    .is_some_and(|t| Arc::ptr_eq(t.arc(), target))
        })?;
        // Cast: the forward's address, the word the stub reads.
        let word = &**found as *const NotEntrantForward as usize;
        self.forward.store(word, Ordering::Release);
        Some(if found.cycle {
            ForwardOutcome::InstalledCycle
        } else {
            ForwardOutcome::Installed
        })
    }

    /// STASH20-2: why a call site whose caller roots this record's body may
    /// NOT be re-pointed at `target`, or `None` when it may: some kept
    /// forward of this record owns `target` and none closes a reference
    /// cycle. Such an owner is never released ([`Self::release_forwards_to`]
    /// is reached only through the cache's cycle registry, which names only
    /// records with a cycle forward, and [`Self::note_call_site_rebound`]
    /// stops any later one), so `target` stays mapped for as long as this
    /// record's body, which the rebound caller roots. The caller holds the
    /// cache's `mutation` lock, as every forward install does.
    pub(crate) fn rebind_refusal(&self, target: &Arc<CompiledMethod>) -> Option<&'static str> {
        let kept = self.forwards.lock();
        if kept.iter().any(|f| f.cycle) {
            return Some("the stub has forwarded across a reference cycle");
        }
        let owned = kept.iter().any(|f| {
            f.target
                .lock()
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t.arc(), target))
        });
        (!owned).then_some("no forward of the stub owns the current body")
    }

    /// STASH20-2: count one call site rebound through this record and return
    /// the running count. Under the cache's `mutation` lock.
    pub(crate) fn note_call_site_rebound(&self) -> u32 {
        self.rebinds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1)
    }

    /// STASH20-2: clear the forward word, so the NEXT stale caller's call
    /// comes through the VM once and can be rebound too (HotSpot resolves
    /// every call site once through its wrong-method stub), while a forward
    /// can still be published again: only while the record is below
    /// [`NOT_ENTRANT_MAX_FORWARDS`] (`may_forward_to`'s cap), so the next
    /// slow call re-publishes the same box ([`Self::reuse_kept_forward`]).
    /// The box stays, for a thread that read the word a moment ago. Returns
    /// whether the word was cleared.
    pub(crate) fn clear_forward_word_for_rebinds(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.forwards.lock().len() >= NOT_ENTRANT_MAX_FORWARDS {
            return false;
        }
        self.forward.store(0, Ordering::Release);
        true
    }

    /// The lock-free half of [`Self::forward_to_releasable`]'s refusals, asked
    /// before the cache takes its `mutation` lock, so a record that can never
    /// forward to `target` does not take that lock on every slow call.
    pub(crate) fn may_forward_to(&self, target: &CompiledMethod) -> bool {
        use std::sync::atomic::Ordering;
        NOT_ENTRANT_FORWARDING_ENABLED
            && self.body_entry != 0
            && self.admits_forward_target(target)
            && self.refused_too_large.load(Ordering::Relaxed) != target.artifact_id
            && self.forwards.lock().len() < NOT_ENTRANT_MAX_FORWARDS
    }

    /// The static half of [`Self::forward_to`]'s admission.
    fn admits_forward_target(&self, target: &CompiledMethod) -> bool {
        self.forward_target_refusal(target).is_none()
    }

    /// Why [`Self::admits_forward_target`] refuses `target`, or `None` when it
    /// admits it. Each term names one conjunct, for the `CRATONVM_DBG_DEOPT`
    /// line of a record that keeps re-dispatching
    /// ([`Self::explain_forward_refusal`]).
    fn forward_target_refusal(&self, target: &CompiledMethod) -> Option<&'static str> {
        use std::sync::atomic::Ordering;
        // Cast: an address, compared only.
        let entry = target.entry_ptr() as usize;
        if entry == 0 {
            return Some("target has no entry");
        }
        if entry == self.body_entry || target.artifact_id == self.body_artifact_id {
            return Some("target is the patched body itself");
        }
        // Cast: an address, tested for the inline-cache context tag.
        if (entry as u64) & crate::JIT_IC_NEEDS_CONTEXT_TAG != 0 {
            return Some("target entry carries the inline-cache context tag");
        }
        if target.needs_context != self.needs_context {
            return Some("context-pointer ABI differs");
        }
        if target.entry_java_args == u16::MAX
            || usize::from(target.entry_java_args) != self.num_java_args
        {
            return Some("argument count differs");
        }
        if target.requires_wrapped_entry {
            return Some("target requires the wrapped entry");
        }
        if target.has_indy_trap {
            return Some("target holds an invokedynamic trap");
        }
        if target.retired.load(Ordering::SeqCst) {
            return Some("target retired");
        }
        if target.superseded.load(Ordering::SeqCst) {
            return Some("target superseded");
        }
        if target.is_not_entrant() {
            return Some("target is itself not entrant");
        }
        if let Some(reason) = caller_held_forward_refusal(self.caller_held_bound, target) {
            return Some(reason);
        }
        // Wave 24: a GPU kernel keeps its dispatch helper, where the
        // offload hook lives -- what the direct-call planning of both
        // tiers asks before it bakes a static call, and what a caller
        // compiled before the kernel was known could not have asked.
        if self.info.invoke_kind == 3
            && crate::offload_hook::keeps_dispatch_helper(
                self.info.class_name,
                self.info.method_name,
                self.info.descriptor,
            )
        {
            return Some("method keeps its dispatch helper (offload)");
        }
        let key_matches = target.published_key.get().is_some_and(
            |(class_name, method_name, descriptor, class_id)| {
                &**class_name == self.info.class_name
                    && &**method_name == self.info.method_name
                    && &**descriptor == self.info.descriptor
                    && *class_id == self.info.declaring_class_id
            },
        );
        if !key_matches {
            return Some("target not published under this record's key");
        }
        None
    }

    /// Why [`Self::forward_to`] would refuse `target` now (diagnostics only:
    /// the `CRATONVM_DBG_DEOPT` line of a record that keeps re-dispatching).
    /// `"admitted"` when nothing refuses it.
    pub fn explain_forward_refusal(&self, target: &Arc<CompiledMethod>) -> &'static str {
        use std::sync::atomic::Ordering;
        if !NOT_ENTRANT_FORWARDING_ENABLED || self.body_entry == 0 {
            return "forwarding off for this record";
        }
        if let Some(reason) = self.forward_target_refusal(target) {
            return reason;
        }
        if self.forwards.lock().len() >= NOT_ENTRANT_MAX_FORWARDS {
            return "forward cap reached";
        }
        if self.refused_too_large.load(Ordering::Relaxed) == target.artifact_id {
            return "cycle walk passed the reach limit";
        }
        if self.refused_target.load(Ordering::Relaxed) == target.artifact_id {
            return "target reaches the patched body (cycle; refused by the bare form)";
        }
        "admitted"
    }

    /// The body the stub forwards to now, if any.
    #[cfg(test)]
    fn forward_target(&self) -> Option<Arc<CompiledMethod>> {
        let current = self.forward.load(std::sync::atomic::Ordering::Acquire);
        if current == 0 {
            return None;
        }
        // SAFETY: as in `forward_to`.
        let cur = unsafe { &*(current as *const NotEntrantForward) };
        cur.target.lock().as_ref().map(|t| Arc::clone(t.arc()))
    }
}

/// Does `from` reach the artifact `artifact_id` through baked callee roots or
/// not-entrant forwards, at any depth? `true` also when the walk cannot tell
/// (too many bodies, or another record's forward list is busy): the caller
/// refuses a forward on `true`, which only keeps the VM path.
fn reaches_artifact(from: &Arc<CompiledMethod>, artifact_id: u64) -> Reach {
    let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
    let mut stack: Vec<Arc<CompiledMethod>> = vec![Arc::clone(from)];
    while let Some(body) = stack.pop() {
        if body.artifact_id == artifact_id {
            return Reach::Yes;
        }
        if !seen.insert(body.artifact_id) {
            continue;
        }
        if seen.len() > FORWARD_REACH_LIMIT {
            return Reach::TooLarge;
        }
        for root in &body._direct_callee_roots {
            stack.push(Arc::clone(root.arc()));
        }
        if let Some(record) = body.not_entrant.get() {
            let Some(forwards) = record.forwards.try_lock() else {
                return Reach::Busy;
            };
            for forward in forwards.iter() {
                let Some(target) = forward.target.try_lock() else {
                    return Reach::Busy;
                };
                if let Some(target) = target.as_ref() {
                    stack.push(Arc::clone(target.arc()));
                }
            }
        }
    }
    Reach::No
}

impl NotEntrantRecord {
    /// The Java arguments of the call the stub intercepted, receiver first, in
    /// the raw compiled-ABI encoding (references as addresses, floating point
    /// as bits).
    ///
    /// # Safety
    ///
    /// `regs` must point at the [`NOT_ENTRANT_ARG_REGS`] spilled argument
    /// registers and `stack_args` at the caller's stack-passed arguments, as
    /// the stub passes them, for a call made into the body this record
    /// belongs to (so every argument past the register file is really there).
    pub unsafe fn java_args(&self, regs: *const i64, stack_args: *const i64) -> Vec<i64> {
        let skip = usize::from(self.needs_context);
        (0..self.num_java_args)
            .map(|j| {
                let abi = j + skip;
                if abi < NOT_ENTRANT_ARG_REGS {
                    // SAFETY: the caller guarantees `NOT_ENTRANT_ARG_REGS`
                    // readable words at `regs`.
                    unsafe { regs.add(abi).read() }
                } else {
                    // SAFETY: the caller guarantees the stack-passed argument
                    // `abi - NOT_ENTRANT_ARG_REGS` exists: the body's own
                    // prologue would read the same slot.
                    unsafe { stack_args.add(abi - NOT_ENTRANT_ARG_REGS).read() }
                }
            })
            .collect()
    }

    /// The VM context pointer the caller passed, when the body takes one.
    ///
    /// # Safety
    ///
    /// As [`Self::java_args`].
    pub unsafe fn context_arg(&self, regs: *const i64) -> Option<i64> {
        // SAFETY: the caller guarantees at least one readable word at `regs`.
        self.needs_context.then(|| unsafe { regs.read() })
    }
}

/// The two addresses a not-entrant stub bakes besides its record: the VM's
/// `jit_not_entrant_entry` and the `SharedVm` it re-dispatches in. Per VM,
/// armed on its [`JitCache`] by the VM ([`JitCache::arm_not_entrant`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotEntrantHook {
    /// `extern "C" fn(record: i64, regs: i64, stack_args: i64) -> i64`.
    pub helper: usize,
    /// `*const SharedVm`.
    pub vm_ptr: usize,
}

/// Why a body could not be made not entrant. Every refusal leaves the body
/// exactly as it was (entry unpatched), which is the pre-wave-22 behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotEntrantRefusal {
    /// Not x86-64: the patch protocol is not implemented here.
    Unsupported,
    /// The body does not open with [`ENTRY_PATCH_PAD`] (an adapter, a stub, a
    /// hand-built fixture, an aarch64 body).
    NoEntryPad,
    /// The entry is not the 8-aligned start of the body's mapping.
    Unaligned,
    /// Never published, or its producer did not stamp its argument count:
    /// nothing says which method to re-dispatch.
    NoIdentity,
    /// The stamped argument count fits neither a static nor an instance
    /// method of the published descriptor.
    ArityMismatch,
    /// The mapping has no room past the code for the stub.
    NoTail,
    /// The OS refused to make the mapping writable.
    Protect,
}

impl NotEntrantRefusal {
    /// Every refusal, in [`Self::index`] order.
    pub const ALL: [NotEntrantRefusal; 7] = [
        NotEntrantRefusal::Unsupported,
        NotEntrantRefusal::NoEntryPad,
        NotEntrantRefusal::Unaligned,
        NotEntrantRefusal::NoIdentity,
        NotEntrantRefusal::ArityMismatch,
        NotEntrantRefusal::NoTail,
        NotEntrantRefusal::Protect,
    ];

    /// This refusal's slot in the cache's per-reason counters (wave 23).
    pub fn index(self) -> usize {
        match self {
            NotEntrantRefusal::Unsupported => 0,
            NotEntrantRefusal::NoEntryPad => 1,
            NotEntrantRefusal::Unaligned => 2,
            NotEntrantRefusal::NoIdentity => 3,
            NotEntrantRefusal::ArityMismatch => 4,
            NotEntrantRefusal::NoTail => 5,
            NotEntrantRefusal::Protect => 6,
        }
    }
}

/// Slots of [`JitCache`]'s per-reason not-entrant refusal counters: one per
/// [`NotEntrantRefusal`], then the bodies refused because they were still
/// published ([`NOT_ENTRANT_REFUSED_PUBLISHED`]).
pub(crate) const NOT_ENTRANT_REFUSAL_SLOTS: usize = 8;

/// The counter slot of a body [`JitCache::make_not_entrant`] skipped because
/// it was still published.
pub(crate) const NOT_ENTRANT_REFUSED_PUBLISHED: usize = 7;

/// The x86-64 not-entrant stub for `record`, calling `helper`. Entered by the
/// patched entry's `JMP`, i.e. with the caller's return address on top of the
/// stack and the call's arguments in the ABI registers and on the caller's
/// stack, exactly as the body's own prologue would have found them.
///
/// With a non-zero `forward_word` (the address of the record's forward word,
/// wave 23) the stub opens with the forwarding prefix. It touches only R10 and
/// R11, which are call-clobbered and carry nothing into a compiled entry on
/// either ABI (every prologue reads its arguments from the ABI argument
/// registers and the caller's stack), and moves neither RSP nor RBP, so the
/// target's prologue finds the call exactly as the caller made it:
///
/// ```text
/// MOV  R11, forward_word
/// MOV  R11, [R11]           ; 0, or a NotEntrantForward
/// TEST R11, R11
/// JZ   slow
/// MOV  R10, [R11+8]         ; &target.retired
/// CMP  BYTE [R10], 0
/// JNE  slow
/// MOV  R10, [R11+16]        ; &target.superseded
/// CMP  BYTE [R10], 0
/// JNE  slow
/// JMP  [R11]                ; target.entry
/// slow:
/// ```
///
/// The slow path, which is the whole stub without forwarding:
///
/// ```text
/// SUB  RSP, 0x58            ; 32 shadow + 6 spill slots + 8: RSP 16-aligned
/// MOV  [RSP+0x20+8*i], ARGi ; every integer argument register
/// MOV  ARG0, record
/// LEA  ARG1, [RSP+0x20]     ; the spilled registers
/// LEA  ARG2, [RSP+0x58+8+SHADOW] ; the caller's stack-passed arguments
/// MOV  RAX, helper
/// CALL RAX
/// ADD  RSP, 0x58
/// RET                       ; to the caller, with the helper's RAX
/// ```
///
/// No frame is built and RBP, the callee-saved registers and the TLS frame
/// mirror are untouched, so to a stack walk the helper was called from the
/// caller's own call site.
///
/// `flags_adjacent` (interpreter round i1 wave 25, lane L6): a
/// [`CompiledMethod`]'s `superseded` flag is the byte right after its
/// `retired` flag ([`retired_and_superseded_adjacent`]), so one
/// `CMP WORD [R10], 0` tests both and the prefix loses its second load,
/// compare and branch (8 instructions, 32 bytes, instead of 11 and 41). The
/// layout of a `repr(Rust)` struct is the compiler's choice, so the stub asks
/// a live body rather than assuming it.
pub fn not_entrant_stub_bytes(
    record: usize,
    helper: usize,
    forward_word: usize,
    flags_adjacent: bool,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(NOT_ENTRANT_STUB_MAX);
    if forward_word != 0 {
        b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
                                            // Cast: an address, baked as a 64-bit immediate.
        b.extend_from_slice(&(forward_word as u64).to_le_bytes());
        b.extend_from_slice(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
        b.extend_from_slice(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        let jz = b.len();
        b.extend_from_slice(&[0x74, 0x00]); // JZ slow (rel8, patched below)
        b.extend_from_slice(&[0x4D, 0x8B, 0x53, 0x08]); // MOV R10, [R11+8]
        let mut early_outs = vec![jz];
        if flags_adjacent {
            // CMP WORD [R10], 0: `retired` and `superseded` together.
            b.extend_from_slice(&[0x66, 0x41, 0x83, 0x3A, 0x00]);
            early_outs.push(b.len());
            b.extend_from_slice(&[0x75, 0x00]); // JNE slow
        } else {
            b.extend_from_slice(&[0x41, 0x80, 0x3A, 0x00]); // CMP BYTE [R10], 0
            early_outs.push(b.len());
            b.extend_from_slice(&[0x75, 0x00]); // JNE slow
            b.extend_from_slice(&[0x4D, 0x8B, 0x53, 0x10]); // MOV R10, [R11+16]
            b.extend_from_slice(&[0x41, 0x80, 0x3A, 0x00]); // CMP BYTE [R10], 0
            early_outs.push(b.len());
            b.extend_from_slice(&[0x75, 0x00]); // JNE slow
        }
        b.extend_from_slice(&[0x41, 0xFF, 0x23]); // JMP QWORD [R11]
        let slow = b.len();
        for at in early_outs {
            // A rel8 from the end of the two-byte jump; the prefix is at most
            // 41 bytes, so every distance fits.
            // Cast: a distance below 64, as a signed byte.
            b[at + 1] = (slow - (at + 2)) as u8;
        }
    }
    b.extend_from_slice(&[0x48, 0x83, 0xEC, 0x58]); // SUB RSP, 0x58
    #[cfg(target_os = "windows")]
    {
        b.extend_from_slice(&[0x48, 0x89, 0x4C, 0x24, 0x20]); // MOV [RSP+0x20], RCX
        b.extend_from_slice(&[0x48, 0x89, 0x54, 0x24, 0x28]); // MOV [RSP+0x28], RDX
        b.extend_from_slice(&[0x4C, 0x89, 0x44, 0x24, 0x30]); // MOV [RSP+0x30], R8
        b.extend_from_slice(&[0x4C, 0x89, 0x4C, 0x24, 0x38]); // MOV [RSP+0x38], R9
        b.extend_from_slice(&[0x48, 0xB9]); // MOV RCX, imm64 (record)
                                            // Cast: an address, baked as a 64-bit immediate.
        b.extend_from_slice(&(record as u64).to_le_bytes());
        b.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x20]); // LEA RDX, [RSP+0x20]
                                                              // LEA R8, [RSP+0x80]: past the 0x58 reserve, the return address and
                                                              // the caller's 32-byte shadow space.
        b.extend_from_slice(&[0x4C, 0x8D, 0x84, 0x24, 0x80, 0x00, 0x00, 0x00]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        b.extend_from_slice(&[0x48, 0x89, 0x7C, 0x24, 0x20]); // MOV [RSP+0x20], RDI
        b.extend_from_slice(&[0x48, 0x89, 0x74, 0x24, 0x28]); // MOV [RSP+0x28], RSI
        b.extend_from_slice(&[0x48, 0x89, 0x54, 0x24, 0x30]); // MOV [RSP+0x30], RDX
        b.extend_from_slice(&[0x48, 0x89, 0x4C, 0x24, 0x38]); // MOV [RSP+0x38], RCX
        b.extend_from_slice(&[0x4C, 0x89, 0x44, 0x24, 0x40]); // MOV [RSP+0x40], R8
        b.extend_from_slice(&[0x4C, 0x89, 0x4C, 0x24, 0x48]); // MOV [RSP+0x48], R9
        b.extend_from_slice(&[0x48, 0xBF]); // MOV RDI, imm64 (record)
                                            // Cast: an address, baked as a 64-bit immediate.
        b.extend_from_slice(&(record as u64).to_le_bytes());
        b.extend_from_slice(&[0x48, 0x8D, 0x74, 0x24, 0x20]); // LEA RSI, [RSP+0x20]
                                                              // LEA RDX, [RSP+0x60]: past the 0x58 reserve and the return address.
        b.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x60]);
    }
    b.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64 (helper)
                                        // Cast: an address, baked as a 64-bit immediate.
    b.extend_from_slice(&(helper as u64).to_le_bytes());
    b.extend_from_slice(&[0xFF, 0xD0]); // CALL RAX
    b.extend_from_slice(&[0x48, 0x83, 0xC4, 0x58]); // ADD RSP, 0x58
    b.push(0xC3); // RET
    debug_assert!(b.len() <= NOT_ENTRANT_STUB_MAX);
    b
}

/// Is a [`CompiledMethod`]'s `superseded` flag the byte right after its
/// `retired` flag? The same answer for every body (one struct layout), which
/// [`not_entrant_stub_bytes`] uses to test both with one compare
/// (interpreter round i1 wave 25, lane L6).
fn retired_and_superseded_adjacent(body: &CompiledMethod) -> bool {
    // Cast: field addresses, compared only.
    let retired = &body.retired as *const std::sync::atomic::AtomicBool as usize;
    // Cast: as above.
    let superseded = &body.superseded as *const std::sync::atomic::AtomicBool as usize;
    superseded == retired + 1
}

impl CompiledMethod {
    /// Did the compilation that built this body copy bytecode of `class_id`
    /// ([`CompiledMethod::copied_classes`]; interpreter round i1 wave 25, lane
    /// L6)?
    pub(crate) fn copied_bytecode_of(&self, class_id: cratonvm_types::ClassId) -> bool {
        self.copied_classes
            .binary_search(&class_id.as_u32())
            .is_ok()
    }

    /// The classes (ids, sorted) whose bytecode this body's compilation
    /// copied ([`CompiledMethod::copied_classes`]), for the VM's OSR door: a
    /// frame that began before one of them was redefined must not enter a
    /// body that may have folded that class's code (interpreter round i1
    /// wave 25 follow-up, lane L6c).
    pub fn copied_classes(&self) -> &[u32] {
        &self.copied_classes
    }

    /// How many exit-capable polls this body recorded
    /// ([`CompiledMethod::exit_poll_sites`]), for the VM's diagnostics
    /// (interpreter round i1 wave 25 follow-up, lane L6b).
    pub fn exit_poll_site_count(&self) -> usize {
        self.exit_poll_sites.len()
    }

    /// Whether this body's entry has been patched to its not-entrant stub.
    pub fn is_not_entrant(&self) -> bool {
        self.not_entrant.get().is_some()
    }

    /// JIT round 21 wave 8 (lane splice): has this body's not-entrant stub
    /// EVER forwarded a call to another body? `false` for an entrant body. A
    /// stack walk may name a frame entered through this body's entry after
    /// the patch as this body only when the answer is `false`: the stub
    /// itself builds no frame, so a compiled frame entered there was built by
    /// this body's own prologue (before the patch) or by a forward target's
    /// (after one). Fails closed (`true`) when the forward list is busy.
    pub fn not_entrant_stub_ever_forwarded(&self) -> bool {
        self.not_entrant
            .get()
            .is_some_and(|record| record.ever_forwarded())
    }

    /// STASH20-2: has [`JitCache::rebind_stale_call_site`] re-pointed one of
    /// this body's direct `CALL rel32` sites? Then the target a site's
    /// displacement names now is not proof of which body a frame returning
    /// there was entered through (an activation of the old callee may still
    /// be running), so a stack walk that identifies a callee frame by
    /// decoding its caller's call must fail closed for this body.
    pub fn has_rebound_call_sites(&self) -> bool {
        self.rebound_call_sites
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// JIT round 20 wave 20, lane rev16: does a call site
    /// [`JitCache::rebind_stale_call_site`] re-pointed in this body name a
    /// body whose entry `withdrawn` holds? Such a caller bakes that body as
    /// it bakes a [`Self::_direct_callee_entries`] callee, so the
    /// invalidation closures withdraw it with its callee. `false` (one
    /// atomic load) for a body with no rebound site.
    pub(crate) fn rebound_call_reaches(&self, withdrawn: impl Fn(usize) -> bool) -> bool {
        self.has_rebound_call_sites()
            && self
                .rebound_callees
                .lock()
                .iter()
                .any(|&(entry, _)| withdrawn(entry))
    }

    /// JIT round 20 wave 20, lane rev16: the artifact ids of the bodies this
    /// body's rebound call sites name ([`Self::rebound_call_reaches`]).
    pub(crate) fn rebound_callee_artifacts(&self) -> Vec<u64> {
        if !self.has_rebound_call_sites() {
            return Vec::new();
        }
        self.rebound_callees
            .lock()
            .iter()
            .map(|&(_, id)| id)
            .collect()
    }

    /// Was this body ever published through a `JitCache` (`put` / `put_osr`
    /// stamp the key)? `false` for an unpublished body such as the accepted
    /// optimizing OSR body, whose `owner_class_id` is stamped without a
    /// publication (round 12 wave 6, lane jni; applied in wave 7, lane
    /// replay4).
    pub fn was_published(&self) -> bool {
        self.published_key.get().is_some()
    }

    /// Whether a class redefinition withdrew this body because it may run
    /// the redefined class's old bytecode (interpreter round i1 wave 23, lane
    /// L6): set by [`JitCache::make_not_entrant`] for every stale body it
    /// reached, whether or not its entry could be patched, and by
    /// [`JitCache::mark_withdrawn_by_redefinition`] for the optimizing OSR
    /// bodies the cache never published. A frame running such a body leaves
    /// for the interpreter at its next back-edge poll when its exit can be
    /// resumed (the VM's `polling_body_must_leave`), which is what reaches a
    /// frame already running a splice of the redefined callee.
    pub fn is_withdrawn_by_redefinition(&self) -> bool {
        self.withdrawn_by_redefinition
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Was this single-pass OSR body armed for the OSR profit exit
    /// (`JitCache::arm_osr_profit_exit`; JIT round 23 wave 1, lane L1)? The
    /// OSR door neither re-enters nor reuses an armed body, and its transfer
    /// sink takes an `OsrExit` of one as committed.
    pub fn osr_profit_exit_armed(&self) -> bool {
        self.osr_profit_exit_armed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record that a compiled caller binds this body for a caller-held
    /// synchronized `CALL` (the VM's `sync_direct_target`; interpreter round
    /// i2 wave 53, lane L2). Called BEFORE the caller's code that `CALL`s it
    /// can be published, so a thread that reached that code through its
    /// publication sees the mark. From then on the VM never sets
    /// [`crate::SAFEPOINT_VERDICT_NAMED_BODY`] for this body: its
    /// [`Self::named_poll_exits_only`] polls stay verdict-blind, which is what
    /// the caller-held site's cold side (every sentinel is the callee's
    /// exception) needs. Never cleared.
    pub fn mark_bound_by_a_caller_held_site(&self) {
        self.bound_by_a_caller_held_site
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Has a compiled caller bound this body for a caller-held synchronized
    /// `CALL` ([`Self::mark_bound_by_a_caller_held_site`])?
    pub fn is_bound_by_a_caller_held_site(&self) -> bool {
        self.bound_by_a_caller_held_site
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Was this OSR body forced as its own class's obsolete activation after
    /// a redefinition that renumbered the class's constant pool
    /// ([`JitCache::force_withdrawn_exit_polls_after`]; interpreter round i1
    /// wave 45, lane L2)? The VM's verdict then lets its frame leave for the
    /// interpreter at the body's next exit, where the frame -- the OSR door's
    /// live interpreter frame, already moved onto its translated body by the
    /// obsolete-frame machinery -- runs the rest of the activation against
    /// the right constants, instead of every constant-pool helper of the body
    /// translating its index for the rest of the activation.
    pub fn leaves_as_renumbered_obsolete(&self) -> bool {
        self.leaves_as_renumbered_obsolete
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is this an OSR body (a single-pass OSR compile, or an optimizing body
    /// with OSR entry stubs)? The force pass's split between the wave-45
    /// rule (the OSR door transfers the exit into its own frame) and the
    /// wave-46 one (a stash sink rebuilds the frame).
    pub fn is_osr_body(&self) -> bool {
        self.compiled_via_osr || !self.ir_osr_entries.is_empty()
    }

    /// [`Self::leaves_as_renumbered_obsolete`] for a METHOD-ENTRY body
    /// (interpreter round i1 wave 46, lane L2;
    /// [`OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`]): its exit is resumed by
    /// a stash sink, which rebuilds the frame in [`Self::compiled_source`]
    /// and restamps it with [`Self::compile_cp_stamp`], so the VM asks, once,
    /// whether that frame can be moved onto the class's current pool
    /// ([`Self::renumbered_entry_resumes`]) before it lets the body leave.
    pub fn leaves_as_renumbered_obsolete_entry(&self) -> bool {
        self.leaves_as_renumbered_obsolete() && !self.is_osr_body()
    }

    /// The VM's recorded answer for a method-entry body marked to leave
    /// ([`Self::leaves_as_renumbered_obsolete_entry`]): `None` while it was
    /// not asked since the force pass marked the body.
    pub fn renumbered_entry_resumes(&self) -> Option<bool> {
        match self
            .renumbered_entry_resumes
            .load(std::sync::atomic::Ordering::Acquire)
        {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    /// The VM's recorded answer for a method-entry body of a class redefined
    /// since it was compiled, asked under an interpreter-only request
    /// ([`INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`]; interpreter round i1 wave
    /// 47, lane L2), when it was recorded at the process's redefinition count
    /// `count`; `None` when it was never asked or asked before the count
    /// moved.
    pub fn obsolete_entry_resumes_at(&self, count: u64) -> Option<bool> {
        let memo = self
            .obsolete_entry_resumes_memo
            .load(std::sync::atomic::Ordering::Acquire);
        if memo == 0 || memo >> 2 != count {
            return None;
        }
        Some(memo & 3 == 1)
    }

    /// Record the VM's answer ([`Self::obsolete_entry_resumes_at`]) at the
    /// redefinition count `count`.
    pub fn note_obsolete_entry_resumes_at(&self, count: u64, resumes: bool) {
        self.obsolete_entry_resumes_memo.store(
            (count << 2) | if resumes { 1 } else { 2 },
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Record the VM's answer ([`Self::renumbered_entry_resumes`]).
    pub fn note_renumbered_entry_resumes(&self, resumes: bool) {
        self.renumbered_entry_resumes.store(
            if resumes { 1 } else { 2 },
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Can a stash sink resume a frame of this METHOD-ENTRY body as its
    /// class's obsolete activation (wave 46, lane L2)? It needs the bytecode
    /// the body was compiled from ([`Self::compiled_source`], stamped by the
    /// VM's publish sites) and that bytecode's pool generation
    /// ([`Self::compile_cp_stamp`]), and the method must neither be
    /// `ACC_SYNCHRONIZED` nor hold a monitor anywhere in its code: the
    /// call-site service refuses the former, and every sink's precise-resume
    /// gate (`sink_precise_resume_allowed` in the VM) the latter, and a
    /// refused exit is re-run from entry.
    ///
    /// Public since interpreter round i1 wave 47 (lane L2): the VM's verdict
    /// asks it for a body an interpreter-only request needs to leave
    /// ([`INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`]).
    pub fn entry_resumable_as_obsolete(&self) -> bool {
        self.compile_cp_stamp.is_some()
            && self.compiled_source().is_some_and(|source| {
                !source.is_synchronized
                    && !crate::bytecode_holds_monitor(&source.code, source.code.len())
            })
    }

    /// [`Self::entry_resumable_as_obsolete`] as an interpreter-only request
    /// asks it ([`INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE`]): also a
    /// method that holds a monitor. An `ACC_SYNCHRONIZED` one, whose method
    /// monitor only a sink that hands it over may resume -- a door, which
    /// holds it in its guard, or any sink for a self-locking body, whose
    /// exits name it -- which the VM's verdict asks per activation
    /// (`helpers::interpreter_only_obsolete_source`); and one with
    /// `monitorenter` in its code when the body vouches for its frames
    /// ([`Self::can_deopt_resume`]: its points name every lock they hold,
    /// which the rebuilt frame keeps and releases at its own `monitorexit`).
    /// The wave-46 renumbering rule keeps asking
    /// [`Self::entry_resumable_as_obsolete`]: a redefinition alone lets an
    /// obsolete activation finish compiled, as HotSpot's does.
    pub fn entry_resumable_as_obsolete_for_the_interpreter(&self) -> bool {
        if !INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE {
            return self.entry_resumable_as_obsolete();
        }
        self.compile_cp_stamp.is_some()
            && self.compiled_source().is_some_and(|source| {
                !crate::bytecode_holds_monitor(&source.code, source.code.len())
                    || self.can_deopt_resume
            })
    }

    /// Stamp the key this body is published under. Called by
    /// [`JitCache::put`] / [`JitCache::put_osr`]; the first publication wins.
    pub(crate) fn stamp_published_key(
        &self,
        class_name: &Arc<str>,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        class_id: u32,
    ) {
        let _ = self.published_key.set((
            Arc::clone(class_name),
            Arc::clone(method_name),
            Arc::clone(descriptor),
            class_id,
        ));
    }

    /// The key this body was published under (`(class, method, descriptor,
    /// class id)`; [`Self::stamp_published_key`]), `None` for a body never
    /// published. Cloned: the strings are `Arc`s.
    pub fn published_key(&self) -> Option<(Arc<str>, Arc<str>, Arc<str>, u32)> {
        self.published_key.get().cloned()
    }

    /// Were `self` and `other` both published under the same key (class name,
    /// method name, descriptor and class id; [`Self::stamp_published_key`])?
    /// `false` when either was never published. HANDLER11-2 (JIT round 19
    /// wave 11, lane handler11): the VM's doors ask it of the body they
    /// entered, once that body is not entrant, and the body whose trap they
    /// took -- the one its stub forwarded the call to is the same method's
    /// current body ([`NotEntrantRecord::forward_to`]).
    pub fn published_as_same_method(&self, other: &CompiledMethod) -> bool {
        match (self.published_key.get(), other.published_key.get()) {
            (Some(a), Some(b)) => a.3 == b.3 && *a.0 == *b.0 && *a.1 == *b.1 && *a.2 == *b.2,
            _ => false,
        }
    }

    /// The class id this body was published under ([`Self::stamp_published_key`]),
    /// `None` for a body never published. HANDLER11-4 (JIT round 19 wave 12,
    /// lane handler12): a call-site service asks whether that class is one the
    /// site can reach before it claims the body's stashed frame.
    pub fn published_class_id(&self) -> Option<u32> {
        self.published_key.get().map(|key| key.3)
    }

    /// The whole key this body was published under ([`Self::stamp_published_key`]):
    /// `(class, method, descriptor, class id)`, `None` for a body never
    /// published. JIT round 20 wave 11, lane pLate (W16L1-3): the VM's
    /// not-entrant slow path names the stale CALLER it returns into by it, to
    /// ask one recompile of that method.
    pub fn published_key_parts(&self) -> Option<(Arc<str>, Arc<str>, Arc<str>, u32)> {
        self.published_key
            .get()
            .map(|(class, method, descriptor, id)| {
                (
                    Arc::clone(class),
                    Arc::clone(method),
                    Arc::clone(descriptor),
                    *id,
                )
            })
    }

    /// Patch this body's entry so every later call into it -- from any baked
    /// caller -- is re-dispatched by the VM instead of running this code. See
    /// the module header. Idempotent; a refusal changes nothing.
    ///
    /// Not thread-safe against itself: two concurrent patches of one body
    /// would each write a stub naming their own record. The caller holds the
    /// owning cache's mutation lock ([`JitCache::make_not_entrant`]) or owns
    /// the body outright (the tests).
    pub(crate) fn make_not_entrant(&self, hook: NotEntrantHook) -> Result<(), NotEntrantRefusal> {
        if self.is_not_entrant() {
            return Ok(());
        }
        let record = self.not_entrant_record(hook)?;
        self.install_not_entrant_stub(record, hook.helper)
    }

    /// The record a not-entrant stub of this body would hand the VM: the
    /// method it was published as, static or not by its stamped argument
    /// count. Refused when nothing names the method.
    fn not_entrant_record(
        &self,
        hook: NotEntrantHook,
    ) -> Result<Box<NotEntrantRecord>, NotEntrantRefusal> {
        let Some((class_name, method_name, descriptor, class_id)) = self.published_key.get() else {
            return Err(NotEntrantRefusal::NoIdentity);
        };
        if self.entry_java_args == u16::MAX {
            return Err(NotEntrantRefusal::NoIdentity);
        }
        let declared = crate::count_param_slots(descriptor);
        let n = usize::from(self.entry_java_args);
        let is_static = if n == declared {
            true
        } else if n == declared + 1 {
            false
        } else {
            return Err(NotEntrantRefusal::ArityMismatch);
        };
        let mut record = Box::new(NotEntrantRecord::new(
            class_name,
            method_name,
            descriptor,
            *class_id,
            is_static,
            hook.vm_ptr,
            self.needs_context,
            n,
        ));
        // HANDLER11-5: a caller-held site's body forwards only to a body
        // bound the same way.
        record.caller_held_bound = self.is_bound_by_a_caller_held_site();
        Ok(record)
    }

    /// The patch itself: write the stub for `record` into the mapping's tail,
    /// then swap the entry pad for a `JMP` to it. The record is kept only when
    /// the entry was patched. Same exclusion rule as [`Self::make_not_entrant`].
    ///
    /// Three steps since wave 24 (lane L6), so [`JitCache::make_not_entrant`]
    /// can change the protection of a run of adjacent mappings once instead of
    /// twice per body: [`Self::prepare_not_entrant_patch`] (every check, no
    /// write), a protection change, [`Self::commit_not_entrant_patch`].
    pub(crate) fn install_not_entrant_stub(
        &self,
        record: Box<NotEntrantRecord>,
        helper: usize,
    ) -> Result<(), NotEntrantRefusal> {
        let patch = self.prepare_not_entrant_patch(record, helper)?;
        let (base, mapped) = (patch.base, patch.mapped);
        if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
            return Err(NotEntrantRefusal::Protect);
        }
        // SAFETY: the patch was prepared for this body, and its mapping is
        // writable now.
        unsafe { self.commit_not_entrant_patch(patch) };
        restore_read_execute(base, mapped);
        Ok(())
    }

    /// Every check of the patch and everything it will write, with nothing
    /// written yet: the record stamped with this body's identity, the stub
    /// assembled for it, and the entry word that jumps there.
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    fn prepare_not_entrant_patch(
        &self,
        mut record: Box<NotEntrantRecord>,
        helper: usize,
    ) -> Result<PreparedNotEntrantPatch, NotEntrantRefusal> {
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (record, helper);
            Err(NotEntrantRefusal::Unsupported)
        }
        #[cfg(target_arch = "x86_64")]
        {
            // Cast: addresses, for alignment and offset arithmetic.
            let base = self._buffer.as_ptr() as usize;
            // Cast: as above.
            let entry = self.entry as usize;
            if entry != base || entry % 8 != 0 {
                return Err(NotEntrantRefusal::Unaligned);
            }
            let len = self._buffer.pos();
            if len < 8 {
                return Err(NotEntrantRefusal::NoEntryPad);
            }
            // SAFETY: `[entry, entry + 8)` lies inside the emitted code (`len
            // >= 8`), which is mapped and readable while `self` is alive.
            let head: [u8; 8] = unsafe { std::ptr::read(entry as *const [u8; 8]) };
            if head[..5] != ENTRY_PATCH_PAD {
                return Err(NotEntrantRefusal::NoEntryPad);
            }
            let mapped = self._buffer.mapped_len();
            let tail = (len + 15) & !15usize;
            // The identity the forward's admission and cycle check compare
            // against (wave 23). Written before any address is taken: the
            // box's contents never move.
            record.body_artifact_id = self.artifact_id;
            record.body_entry = entry;
            // Cast: the record's address, baked into the stub.
            let record_ptr = &*record as *const NotEntrantRecord as usize;
            let forward_word = if NOT_ENTRANT_FORWARDING_ENABLED {
                // Cast: the forward word's address, baked into the stub.
                &record.forward as *const std::sync::atomic::AtomicUsize as usize
            } else {
                0
            };
            let stub = not_entrant_stub_bytes(
                record_ptr,
                helper,
                forward_word,
                retired_and_superseded_adjacent(self),
            );
            if tail + stub.len() > mapped {
                return Err(NotEntrantRefusal::NoTail);
            }
            // `JMP rel32` from the end of the five-byte pad to the stub.
            let Ok(rel) = i32::try_from(tail - ENTRY_PATCH_PAD.len()) else {
                return Err(NotEntrantRefusal::NoTail);
            };
            let mut word = head;
            word[0] = 0xE9; // JMP rel32
            word[1..5].copy_from_slice(&rel.to_le_bytes());
            Ok(PreparedNotEntrantPatch {
                record,
                stub,
                base,
                mapped,
                tail,
                word: u64::from_le_bytes(word),
            })
        }
    }

    /// Write a patch [`Self::prepare_not_entrant_patch`] prepared for this
    /// body: the stub into the tail, the record into the body, then the entry
    /// word, in that order.
    ///
    /// # Safety
    ///
    /// `patch` was prepared for `self` (by this pass, with the same exclusion
    /// rule as [`Self::make_not_entrant`]), and `[patch.base, patch.base +
    /// patch.mapped)` is writable now.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    unsafe fn commit_not_entrant_patch(&self, patch: PreparedNotEntrantPatch) {
        let PreparedNotEntrantPatch {
            record,
            stub,
            base,
            mapped: _,
            tail,
            word,
        } = patch;
        // SAFETY: `[base + tail, base + tail + stub.len())` is inside the
        // mapping (checked by the preparation), past every emitted byte, so no
        // thread executes it yet, and the mapping is writable (the caller).
        unsafe {
            std::ptr::copy_nonoverlapping(stub.as_ptr(), (base + tail) as *mut u8, stub.len());
        }
        // `is_not_entrant()` holds BEFORE the entry can reach the stub (wave
        // 23): the root walk stops trusting this body's entry for a frame's
        // identity as soon as a call may be forwarded elsewhere
        // (`conservative_roots::innermost_frame_method`). Early is the safe
        // direction (it only fails closed). Moving the box into the cell does
        // not move the record the stub bakes.
        let _ = self.not_entrant.set(record);
        // The stub's bytes and the record are globally visible before the jump
        // to them.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the entry is `base`, 8-aligned (checked by the preparation),
        // and `[base, base + 8)` is inside the writable mapping. One aligned
        // 8-byte store: a thread fetching the entry sees the whole pad or the
        // whole `JMP`, and bytes 5..8 (the prologue's first bytes) are
        // rewritten with their own values.
        unsafe {
            (*(base as *const std::sync::atomic::AtomicU64))
                .store(word, std::sync::atomic::Ordering::SeqCst);
        }
        // The stub lies past the region the body was published with, so a
        // profiler sample or a crash in it named nothing (wave 23). The perf
        // map and jitdump only: the gdb sink's registrations are withdrawn by
        // the buffer's emit range, which may not reach here.
        if crate::perf_map::enabled() || crate::jitdump::enabled() {
            let name = crate::code_events::sanitize_name(&format!(
                "not-entrant-stub {}",
                self.method_label
            ));
            crate::perf_map::record(base + tail, stub.len(), &name);
            crate::jitdump::record_load(base + tail, stub.len(), &name);
        }
    }
}

/// A not-entrant patch every check passed, not written yet
/// ([`CompiledMethod::prepare_not_entrant_patch`]; interpreter round i1 wave
/// 24, lane L6): the body's mapping `[base, base + mapped)` (the entry is
/// `base`), the stub and the offset it goes to, the entry word, and the record
/// the stub bakes (boxed, so its address holds when it moves into the body).
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
struct PreparedNotEntrantPatch {
    record: Box<NotEntrantRecord>,
    stub: Vec<u8>,
    base: usize,
    mapped: usize,
    tail: usize,
    word: u64,
}

/// Kill switch for forcing the exit polls of a body a class redefinition
/// withdrew (interpreter round i1 wave 25, lane L6;
/// [`CompiledMethod::force_exit_polls`]). `false`: a frame running such a body
/// leaves only at a poll it takes while a pause is requested -- the wave-23/24
/// behaviour, which misses a frame whose thread was in a call (blocked, or the
/// redefining thread itself) during the redefinition's handshake.
pub const FORCE_WITHDRAWN_EXIT_POLLS_ENABLED: bool = true;

/// Switch for letting an OSR body of the redefined class's OWN bytecode leave
/// for the interpreter when the redefinition renumbered the class's constant
/// pool (interpreter round i1 wave 45, lane L2;
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// Such a body is the class's obsolete activation (JEP 109 lets it finish on
/// its old bytecode), and until wave 45 it was always spared: never forced,
/// and told "stay" at every exit, so every constant-pool helper it reached
/// translated its index into the class's current pool for the rest of the
/// activation (`vm::jit::helpers::CpSite`, where waves 21, 43 and 44 found
/// races). On: it is forced like a body a redefinition of another class
/// withdrew -- its back-edge polls, post-call exits and constant-pool helper
/// exits -- and marked ([`CompiledMethod::leaves_as_renumbered_obsolete`]),
/// so the VM's verdict sends its frame to the interpreter at the next one.
/// An OSR body's frame is the OSR door's live interpreter frame, which the
/// exit transfers into in place (both tiers' door sinks), and which the
/// obsolete-frame machinery has moved, or moves before its next bytecode,
/// onto its body translated into the merged pool: the rest of the activation
/// runs the old bytecode, interpreted, against the old constants -- the
/// HotSpot shape. A METHOD-ENTRY body is not covered here: its exit is
/// resumed by the stash sinks, which since wave 46 rebuild it in its own
/// bytecode ([`OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`]). A redefinition
/// that moved no constant keeps the spare (nothing to translate).
///
/// `false`: every own-class body is spared, as in wave 44.
pub const OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE: bool = true;

/// Switch for letting a METHOD-ENTRY body of the redefined class's OWN
/// bytecode leave for the interpreter when the redefinition renumbered the
/// class's constant pool (interpreter round i1 wave 46, lane L2;
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// [`OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`]'s rule for the bodies it left
/// out. A method-entry body has no interpreter frame of its own: its exit is
/// stashed and resumed by whichever sink its caller reaches -- an interpreter
/// door (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`),
/// a compiled caller's call-site service (`helpers::try_resume_trapped_callee`)
/// or the eager first-call door. Each of them now rebuilds the frame of such
/// a body in the bytecode it was compiled from ([`CompiledMethod::compiled_source`]),
/// restamped with its pool generation, when the exit was one the safepoint
/// verdict granted this body (the grant carries the source); the frame is
/// then moved onto that bytecode translated into the merged pool and runs
/// the rest of its old activation interpreted, against the old constants.
///
/// Only a body a sink can resume that way is marked
/// (`CompiledMethod::entry_resumable_as_obsolete`: a compiled source and a
/// pool stamp, not `ACC_SYNCHRONIZED`, no monitor in its code), and the VM's
/// verdict lets a marked body go only after asking, once, whether the frame
/// translates (`obsolete_frames::rebuilt_body_translates`); otherwise it
/// stays, as before. A redefinition that moved no constant keeps the spare.
///
/// `false`: every own-class method-entry body is spared, as in wave 45.
pub const OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE: bool = true;

/// Switch for letting a compiled body of a class redefined since it was
/// compiled -- its class's obsolete activation, which a redefinition spares
/// (`CompiledMethod::exits_spared_as_obsolete`) -- leave for the
/// interpreter when an agent or a debugger request needs the interpreter
/// (interpreter round i1 wave 47, lane L2;
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// Two halves. Here: the interpreter-only withdrawals' force pass
/// ([`JitCache::force_exit_polls_for_the_interpreter`]) no longer spares such
/// a body when its exit can be resumed -- an OSR body (the OSR door transfers
/// the exit into the obsolete activation's own live frame, and its verdict is
/// the innermost frame's, which carries no redefinition clause), or a
/// METHOD-ENTRY body a stash sink can rebuild in the bytecode it was
/// compiled from ([`CompiledMethod::entry_resumable_as_obsolete`]). In the VM:
/// the safepoint verdict hands such a method-entry body's grant that
/// bytecode and its pool stamp once its frame is known to translate onto the
/// class's current pool (`helpers::interpreter_only_obsolete_source`), and
/// `jvmti_events::polling_body_must_leave` lets a body with a source leave
/// instead of refusing every body compiled before its class's last
/// redefinition; the wave-46 sinks resume the exit there.
///
/// `false`: such a body stays spared and finishes its activation compiled,
/// as in wave 46.
pub const INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE: bool = true;

/// Shape 1 (b) of
/// `docs/feature-designs/jit-ir-optimizations.md`:
/// under [`INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`], an obsolete
/// METHOD-ENTRY body of a method that holds a monitor leaves too
/// ([`CompiledMethod::entry_resumable_as_obsolete_for_the_interpreter`]): an
/// `ACC_SYNCHRONIZED` one when the sink that will resume its activation hands
/// the method monitor over (the VM's verdict asks), one with `monitorenter`
/// in its code when the body vouches for its frames. Before, such a body was
/// spared and finished its activation compiled under a debugger's step or an
/// agent's interpreter-only event, where HotSpot deoptimizes it. `false`
/// restores that.
pub const INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE: bool = true;

/// Why [`CompiledMethod::force_exit_polls`] left a body's polls as they were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitPollRefusal {
    /// Not x86-64: no tier records exit-poll sites there.
    Unsupported,
    /// A recorded site does not hold one of the poll shapes the two x86-64
    /// tiers emit against the recorded flag ([`forced_exit_poll_cc`]).
    /// Nothing of the body is written.
    Shape,
    /// The OS refused to make the mapping writable (the single-body form's
    /// answer; the cache's batch counts it without the variant).
    #[cfg_attr(not(test), allow(dead_code))]
    Protect,
}

/// Is `body` a superseded single-pass OSR sibling of `current`
/// ([`JitCache::superseded_single_pass_osr_bodies`])? Another single-pass OSR
/// body of the same owner class and method label, neither armed nor disarmed
/// for a profit exit yet. t1 lane jitea (L1W1-2).
fn superseded_single_pass_osr_sibling(
    current: &Arc<CompiledMethod>,
    body: &Arc<CompiledMethod>,
) -> bool {
    use std::sync::atomic::Ordering;
    !Arc::ptr_eq(current, body)
        && body.compiled_via_osr
        && !body.used_ir_backend
        && body.owner_class_id == current.owner_class_id
        && body.method_label == current.method_label
        && !body.osr_profit_exit_armed.load(Ordering::Acquire)
        && !body.osr_profit_exit_disarmed.load(Ordering::Acquire)
}

/// The byte a forced exit poll's flag-clear branch becomes, or `None` when
/// `code[cc]` is not the condition-code byte of a safepoint poll against
/// `flag` (interpreter round i1 wave 25, lane L6). `code` is a body's emitted
/// bytes, mapped at `code_addr`.
///
/// Both x86-64 tiers poll with `TEST BYTE <flag>, 0xFF` followed at once by a
/// `Jcc rel32` (`0F cc`): `JZ` over the slow path inline (`0x84`), `JNZ` to the
/// slow path in the IR tier's outlined form (`0x85`). The `TEST` is one of
///
/// * `F6 05 <disp32> FF` -- RIP-relative, both tiers; the displacement must
///   resolve to `flag` from the end of the instruction (`cc - 1`);
/// * `41 F6 43 00 FF` -- `TEST BYTE [R11+0], 0xFF`, the single-pass backend's
///   out-of-reach form (`emit_test_mem8_imm8`, which always emits a disp8);
/// * `41 F6 03 FF` -- `TEST BYTE [R11], 0xFF`, the IR tier's.
///
/// Clearing bit 2 of the condition code turns `JZ` into `JO` and `JNZ` into
/// `JNO`. `TEST` always clears OF, so the forced branch always falls into (or
/// always jumps to) the slow path, whatever the flag holds; the displacement
/// and the instruction's length are untouched. One byte, written with one
/// store: a thread executing the poll at that moment runs the old or the new
/// branch, and both are correct code.
///
/// A poll forced already (`JO` / `JNO`) answers its own byte (JIT round 23
/// wave 1, lane L1): the OSR profit exit ([`JitCache::arm_osr_profit_exit`])
/// forces a body's polls without marking it `exit_polls_forced`, so a later
/// redefinition's force pass must find those sites well-shaped and rewrite
/// them to the same byte, not refuse the whole body (and its post-call exit
/// sites) as `Shape`.
pub(crate) fn forced_exit_poll_cc(
    code: &[u8],
    code_addr: usize,
    cc: usize,
    flag: usize,
) -> Option<u8> {
    let op = *code.get(cc)?;
    if !matches!(op, 0x84 | 0x85 | 0x80 | 0x81)
        || cc < 5
        || code[cc - 1] != 0x0F
        || code[cc - 2] != 0xFF
    {
        return None;
    }
    let rip = cc >= 8 && code[cc - 8] == 0xF6 && code[cc - 7] == 0x05 && {
        let disp = i32::from_le_bytes([code[cc - 6], code[cc - 5], code[cc - 4], code[cc - 3]]);
        // Cast: a signed 32-bit displacement, sign-extended to the address width.
        (code_addr + cc - 1).wrapping_add_signed(disp as isize) == flag
    };
    let sp_r11 = cc >= 6 && code[cc - 6..cc - 2] == [0x41, 0xF6, 0x43, 0x00];
    let ir_r11 = code[cc - 5..cc - 2] == [0x41, 0xF6, 0x03];
    (flag != 0 && (rip || sp_r11 || ir_r11)).then_some(op & !0x04)
}

/// The five bytes a forced post-call exit site becomes -- `JMP rel32` to its
/// stub -- or `None` when `code[site..]` is not a site the single-pass tier
/// emitted for `stub` (interpreter round i1 wave 27, lane L6;
/// [`CompiledMethod::post_call_exit_sites`]). `code` is a body's emitted bytes,
/// mapped at `code_addr`.
///
/// Checked: the site holds [`ENTRY_PATCH_PAD`] (so it was never written), its
/// five bytes lie inside one 8-aligned word that is itself inside the emitted
/// code (so one aligned 8-byte store replaces the whole instruction), and the
/// stub, past the site, opens with the `CALL rel32` (`E8`) into the shared
/// verdict tail.
pub(crate) fn post_call_exit_jump(
    code: &[u8],
    code_addr: usize,
    site: usize,
    stub: usize,
) -> Option<[u8; 5]> {
    let end = site.checked_add(ENTRY_PATCH_PAD.len())?;
    if code.get(site..end)? != ENTRY_PATCH_PAD {
        return None;
    }
    let abs = code_addr.checked_add(site)?;
    let word = abs & !7usize;
    if abs - word > 8 - ENTRY_PATCH_PAD.len()
        || word < code_addr
        || word.checked_add(8)? > code_addr.checked_add(code.len())?
    {
        return None;
    }
    if stub < end || code.get(stub).copied() != Some(0xE8) {
        return None;
    }
    let rel = i32::try_from(stub - end).ok()?;
    let mut jmp = [0xE9, 0, 0, 0, 0];
    jmp[1..].copy_from_slice(&rel.to_le_bytes());
    Some(jmp)
}

/// What forcing one body writes (interpreter round i1 wave 25, lane L6; the
/// post-call sites since wave 27): one condition-code byte per exit poll, and
/// one `JMP rel32` per post-call exit site, each as `(address, bytes)`.
/// `post_calls_refused`: a post-call site failed [`post_call_exit_jump`], so
/// none of the body's post-call sites is written (its polls still are).
#[derive(Default)]
struct ExitPollWrites {
    polls: Vec<(usize, u8)>,
    post_calls: Vec<(usize, [u8; 5])>,
    post_calls_refused: bool,
}

impl ExitPollWrites {
    fn is_empty(&self) -> bool {
        self.polls.is_empty() && self.post_calls.is_empty()
    }
}

impl CompiledMethod {
    /// Rewrite every exit poll of this body ([`Self::exit_poll_sites`]) so its
    /// slow path runs whatever the stop-the-world flag holds (interpreter
    /// round i1 wave 25, lane L6). Returns how many sites were written now:
    /// `Ok(0)` for a body with none, or one already forced.
    ///
    /// # Why
    ///
    /// A frame running a body a class redefinition withdrew (one that spliced
    /// the redefined class's old bytecode) leaves for the interpreter at a
    /// back-edge poll's slow path (wave 23), and a poll takes its slow path
    /// only while a pause is requested. The redefinition's handshake reaches
    /// every thread polling at the time, but not a thread that was inside a
    /// call -- blocked in `CountDownLatch.await()`, or the redefining thread
    /// itself, returning from `retransformClasses` -- whose loop then ran the
    /// old splice until some unrelated pause
    /// (`docs/feature-designs/deopt-osr.md`).
    /// The body is withdrawn: no new activation enters it (its entry is not
    /// entrant, its OSR door refuses it), so its polls are only ever reached
    /// again by the frames that should leave. Forced, each of them asks the
    /// slow path at its next back edge, whichever thread it is on and whenever
    /// its call returns -- HotSpot's lazy deoptimization of a marked nmethod
    /// at its next safepoint, without a return barrier.
    ///
    /// All or nothing: every site is checked ([`forced_exit_poll_cc`]) before
    /// anything is written, so a site that does not hold the expected poll
    /// leaves the body untouched. The caller serialises against every other
    /// patch of the body (the cache's mutation lock, or sole ownership).
    ///
    /// The production caller, [`JitCache::force_withdrawn_exit_polls`], does
    /// the same for a batch of bodies with one protection change per run of
    /// adjacent mappings; this single-body form is the tests'.
    #[cfg(test)]
    pub(crate) fn force_exit_polls(&self) -> Result<usize, ExitPollRefusal> {
        let writes = self.exit_poll_writes()?;
        if writes.is_empty() {
            return Ok(0);
        }
        // Cast: an address, for the protection change.
        let base = self._buffer.as_ptr() as usize;
        let mapped = self._buffer.mapped_len();
        if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
            return Err(ExitPollRefusal::Protect);
        }
        // SAFETY: prepared for this body just now, and its mapping is
        // writable.
        let n = unsafe { self.commit_exit_poll_writes(&writes) };
        restore_read_execute(base, mapped);
        Ok(n)
    }

    /// Does this body have anything a redefinition's withdrawal could force:
    /// an exit poll, or (wave 27) a post-call exit site?
    fn has_forceable_exits(&self) -> bool {
        !self.exit_poll_sites.is_empty() || !self.post_call_exit_sites.is_empty()
    }

    /// The writes that force this body's exit polls and post-call exit sites,
    /// every site checked; empty when there is nothing to force (no site, or
    /// forced already).
    fn exit_poll_writes(&self) -> Result<ExitPollWrites, ExitPollRefusal> {
        if !self.has_forceable_exits()
            || self
                .exit_polls_forced
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(ExitPollWrites::default());
        }
        if !cfg!(target_arch = "x86_64") {
            return Err(ExitPollRefusal::Unsupported);
        }
        let code = self._buffer.as_slice();
        // Cast: an address, for the displacement check and the writes.
        let base = code.as_ptr() as usize;
        let mut writes = ExitPollWrites {
            polls: Vec::with_capacity(self.exit_poll_sites.len()),
            post_calls: Vec::with_capacity(self.post_call_exit_sites.len()),
            post_calls_refused: false,
        };
        for &site in &self.exit_poll_sites {
            // Cast: a buffer offset, widened.
            let cc = site as usize;
            let Some(byte) = forced_exit_poll_cc(code, base, cc, self.exit_poll_flag) else {
                return Err(ExitPollRefusal::Shape);
            };
            writes.polls.push((base + cc, byte));
        }
        // A post-call site that fails its check drops the body's post-call
        // sites, not its polls: the polls alone are wave 26's forcing (the
        // frame leaves at its next back edge), and what the loop-exit
        // handshake's retries rely on (`JitCache::every_withdrawal_forced_since`).
        for &(site, stub) in &self.post_call_exit_sites {
            // Cast: buffer offsets, widened.
            let (site, stub) = (site as usize, stub as usize);
            let Some(jmp) = post_call_exit_jump(code, base, site, stub) else {
                writes.post_calls.clear();
                writes.post_calls_refused = true;
                break;
            };
            writes.post_calls.push((base + site, jmp));
        }
        Ok(writes)
    }

    /// Write what [`Self::exit_poll_writes`] prepared and mark the body
    /// forced. Returns how many sites were written (polls and post-call
    /// sites).
    ///
    /// The poll bytes go first, then each post-call site's word, READ at
    /// write time: a word may share bytes with a poll written just before it,
    /// and must carry that byte's new value, not the one it held when the
    /// writes were prepared. A body whose post-call sites were refused is
    /// marked forced all the same: its polls are, and the refused sites
    /// would be refused again.
    ///
    /// # Safety
    ///
    /// `writes` was prepared for `self` under the caller's exclusion, and the
    /// body's mapping is writable now.
    unsafe fn commit_exit_poll_writes(&self, writes: &ExitPollWrites) -> usize {
        use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
        for &(addr, byte) in &writes.polls {
            // SAFETY: `addr` is a checked condition-code byte inside this
            // body's emitted code, which is writable (the caller). One byte
            // store, so a thread fetching the instruction sees the old or the
            // new branch whole.
            unsafe {
                (*(addr as *const AtomicU8)).store(byte, Ordering::SeqCst);
            }
        }
        for &(addr, jmp) in &writes.post_calls {
            let word = addr & !7usize;
            let at = addr - word;
            // SAFETY: `[word, word + 8)` is an 8-aligned word inside this
            // body's emitted code (`post_call_exit_jump` checked it), which is
            // mapped and writable (the caller). The word is replaced whole by
            // a compare-exchange, so the bytes outside the site keep their
            // current values even if some other atomic writer changed one of
            // them since the load (none is known to: the neighbours are the
            // call's post-call check and the successor's code, which only
            // this pass patches); a thread fetching the site sees the whole
            // `NOP` or the whole `JMP`.
            unsafe {
                let cell = &*(word as *const AtomicU64);
                let mut current = cell.load(Ordering::SeqCst);
                loop {
                    let mut bytes = current.to_le_bytes();
                    bytes[at..at + jmp.len()].copy_from_slice(&jmp);
                    match cell.compare_exchange(
                        current,
                        u64::from_le_bytes(bytes),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(now) => current = now,
                    }
                }
            }
        }
        // Interpreter round i6 wave 123, lane L2: whether every post-call
        // site was written, for a debugger write's verdict
        // (`CompiledMethod::debugger_force_verdict`).
        if !writes.post_calls.is_empty() && !writes.post_calls_refused {
            self.post_call_exits_forced.store(true, Ordering::Release);
        }
        self.exit_polls_forced.store(true, Ordering::Release);
        writes.polls.len() + writes.post_calls.len()
    }
}

/// Back to read-execute after a patch. The patch is in place whatever this
/// answers; a refused restore leaves the mapping writable, which is reported
/// and otherwise harmless.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn restore_read_execute(base: usize, len: usize) {
    if let Err(e) = crate::platform::make_executable(base as *mut u8, len) {
        tracing::warn!(
            base = base,
            error = ?e,
            "JIT not-entrant patch: restoring read-execute was refused"
        );
    }
}

/// The x86-64 base page: what an anonymous mapping's length is rounded up to.
/// Only this is used to decide that two bodies' mappings are adjacent
/// ([`adjacent_patch_runs`]); an over-estimate (the arena's
/// `platform::code_page_size` may be 64 KiB) could call two mappings with a
/// foreign page between them adjacent.
const X86_64_BASE_PAGE: usize = 4096;

/// Split `patches`, sorted by `base`, into runs whose mappings follow one
/// another with nothing between them: each mapping's page-rounded end is the
/// next one's base, so one protection change of `[first base, last end)`
/// touches exactly their pages (wave 24, lane L6). A mapping that does not
/// start on a page is a run of its own. Windows: every patch is a run of its
/// own, because `VirtualProtect` refuses a range spanning two allocations.
/// Returns `(start, end)` index ranges into `patches` with the byte range each
/// covers.
fn adjacent_patch_runs(patches: &[(usize, usize)]) -> Vec<(usize, usize, usize, usize)> {
    let round_up = |x: usize| x.div_ceil(X86_64_BASE_PAGE) * X86_64_BASE_PAGE;
    let mut runs = Vec::new();
    let mut i = 0usize;
    while i < patches.len() {
        let (base, mapped) = patches[i];
        let mut end = round_up(base + mapped);
        let mut j = i + 1;
        if !cfg!(target_os = "windows") && base % X86_64_BASE_PAGE == 0 {
            while j < patches.len() && patches[j].0 == end {
                end = round_up(patches[j].0 + patches[j].1);
                j += 1;
            }
        }
        runs.push((i, j, base, end));
        i = j;
    }
    runs
}

/// Is `body` stale for a redefinition of `(class_id, class_name)` by its own
/// record: it was published under the class, inlined from it, or was compiled
/// from its bytecode (its label names it -- a body the by-name callee door
/// published under a receiver's key). A caller that merely CALLS such a body is
/// not: once the callee is not entrant, the caller's call re-dispatches.
fn stale_by_record(body: &CompiledMethod, class_id: u32, class_name: &str) -> bool {
    body.owner_class_id == class_id
        || body.inlined_methods.iter().any(|(cn, _, _)| cn == class_name)
        // Wave 25: its compile copied the class's bytecode.
        || body.copied_classes.binary_search(&class_id).is_ok()
        || body
            .method_label
            .strip_prefix(class_name)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Was `body` compiled from the bytecode of the class `(class_id,
/// class_name)` itself -- published under it, or labelled with one of its
/// methods -- rather than a caller that spliced it? The half of
/// [`stale_by_record`] that is not `inlined_methods`
/// ([`JitCache::force_withdrawn_exit_polls`]'s spare rule; wave 25, lane L6).
fn compiled_from_class(body: &CompiledMethod, class_id: u32, class_name: &str) -> bool {
    body.owner_class_id == class_id
        || body
            .method_label
            .strip_prefix(class_name)
            .is_some_and(|rest| rest.starts_with('.'))
}

impl JitCache {
    /// Arm this cache's not-entrant patches with the VM's helper and its own
    /// `SharedVm` address. Until armed, [`Self::make_not_entrant`] patches
    /// nothing (the unit-test caches, and every run before the VM's first
    /// redefinition).
    pub fn arm_not_entrant(&self, hook: NotEntrantHook) {
        *self.not_entrant_hook.lock() = Some(hook);
    }

    /// Whether [`Self::arm_not_entrant`] armed this cache (t1 wave 6, lane J6:
    /// the VM's startup arming under `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT`
    /// reads it in its test; DBG). One uncontended lock.
    pub fn not_entrant_armed(&self) -> bool {
        self.not_entrant_hook.lock().is_some()
    }

    /// Refuse, from now on, every publication the redefinition of
    /// `(class_id, class_name)` about to be evicted would withdraw
    /// (interpreter round i1 wave 24, lane L6): with `whole_cache`, every
    /// compilation already begun (the flush's install barrier, raised early);
    /// otherwise every compilation already begun that declares or inlined the
    /// class (the scoped eviction's own record and per-class barrier, logged
    /// early). Called BEFORE [`Self::redefinition_stale_bodies`]: a body a
    /// compile of the class's old bytecode published between that scan and
    /// the eviction was withdrawn by the eviction but never collected, so its
    /// entry stayed patchless and a caller that baked it in the same window
    /// kept entering the old code. The eviction repeats both steps; a second
    /// record and a second barrier are harmless.
    ///
    /// Under `mutation`, which every publication holds while it checks both:
    /// a publication either completed before this (the scan then sees it) or
    /// reads them after it (and is refused).
    pub fn fence_redefinition_publications(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        whole_cache: bool,
    ) {
        let _mutation = self.mutation.lock();
        let barrier = crate::bump_jit_install_epoch();
        if whole_cache {
            self.flush_barrier
                .fetch_max(barrier, std::sync::atomic::Ordering::AcqRel);
            return;
        }
        {
            let mut barriers = self.redefinition_barriers.lock();
            let slot = barriers.entry(class_id).or_insert(0);
            *slot = (*slot).max(barrier);
        }
        self.log_invalidation(crate::InvalidationRecord::RedefinedClass {
            class_id,
            class_name: Arc::from(class_name),
        });
    }

    /// Withdraw the bodies that would keep a debugger's breakpoint in a method
    /// of the class `(class_id, class_name)` from being hit, although the
    /// class is not redefined (interpreter round i1 wave 38, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the class's own bodies, the bodies that inlined one of its methods or
    /// copied its bytecode, and the bodies that call into one of those
    /// through a baked direct call. The scoped half of a redefinition's
    /// eviction, in its order, without the per-class redefinition barrier
    /// ([`Self::withdraw_dependents_of_class`] says why):
    ///
    /// 1. the record that refuses a compile of the class already in flight is
    ///    logged under `mutation` BEFORE the scan, so no such compile
    ///    publishes between the scan and the eviction
    ///    ([`Self::fence_redefinition_publications`]'s scoped arm, without
    ///    the barrier);
    /// 2. the bodies stale by their own record, published or reachable through
    ///    a baked caller ([`Self::redefinition_stale_bodies`]), are listed;
    /// 3. the eviction withdraws them from the maps, with their baked callers,
    ///    and clears the inline-cache slots and cells that hold one.
    ///
    /// Returns `(evicted, withdrawn keys, listed bodies)`: the caller demotes
    /// the keys (unless the cache reports its own withdrawals), then hands
    /// the listed bodies to [`Self::make_not_entrant`] and
    /// [`Self::force_withdrawn_exit_polls`], as a redefinition does.
    pub fn withdraw_class_for_the_interpreter(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> (
        usize,
        Vec<crate::WithdrawnMethodKey>,
        Vec<Arc<CompiledMethod>>,
    ) {
        {
            let _mutation = self.mutation.lock();
            self.log_invalidation(crate::InvalidationRecord::RedefinedClass {
                class_id,
                class_name: Arc::from(class_name),
            });
        }
        let stale = self.redefinition_stale_bodies(class_id, class_name, false);
        let (evicted, withdrawn) = self.withdraw_dependents_of_class(class_id, class_name);
        (evicted, withdrawn, stale)
    }

    /// Every body a redefinition of `(class_id, class_name)` makes stale by
    /// its own record ([`stale_by_record`]): published (method-entry and OSR)
    /// or reachable at any depth through a published body's baked callee
    /// roots -- a superseded body a baked caller still enters -- or, since
    /// wave 26, still alive with exit polls although nothing names it (a
    /// superseded or evicted body a frame may still run;
    /// [`Self::note_exit_poll_body`]). Read-only; to
    /// be taken BEFORE the redefinition's eviction, which empties the maps it
    /// reads, and handed to [`Self::make_not_entrant`] after it.
    ///
    /// `whole_cache` (wave 23): EVERY such body ever published under a method
    /// key, whatever its record. For a
    /// redefinition that flushes the whole cache because some compiled code
    /// may depend on the class in a way no body records (the IR tier's
    /// splices, elided constructors, a bound native: the VM's
    /// `JitRealm::scoped_redefinition_admitted`): any body may run the old
    /// bytecode, and a baked caller kept entering a flushed IR body that had
    /// spliced the class. HotSpot deoptimizes every nmethod in that case too
    /// (`CodeCache::mark_all_nmethods_for_evol_deoptimization`, when
    /// `JvmtiExport::all_dependencies_are_recorded()` is false).
    pub fn redefinition_stale_bodies(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        whole_cache: bool,
    ) -> Vec<Arc<CompiledMethod>> {
        let class_id = class_id.as_u32();
        let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        let mut stale: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut stack: Vec<Arc<CompiledMethod>> = Vec::new();
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    stack.push(Arc::clone(cm.arc()));
                }
            }
        }
        // Interpreter round i1 wave 26, lane L6: and every published body with
        // exit polls that is still alive although no map or baked root names
        // it -- superseded by a tier-up or evicted while a frame kept running
        // it. Such a frame runs its splices of the redefined class exactly as
        // a published body's would, and before this nothing marked or forced
        // it (a scoped redefinition did not even withdraw it). Never-published
        // bodies (the VM's optimizing OSR bodies) are the VM's to list, as its
        // memo's are: they have no identity to patch an entry by.
        for body in self.live_exit_poll_bodies() {
            if body.published_key.get().is_some() {
                stack.push(body);
            }
        }
        while let Some(body) = stack.pop() {
            if !seen.insert(body.artifact_id) {
                continue;
            }
            for root in &body._direct_callee_roots {
                stack.push(Arc::clone(root.arc()));
            }
            // A forwarding stub's target is baked too (wave 23): a caller
            // that enters an older patched body reaches it.
            if let Some(record) = body.not_entrant.get() {
                for forward in record.forwards.lock().iter() {
                    if let Some(target) = forward.target.lock().as_ref() {
                        stack.push(Arc::clone(target.arc()));
                    }
                }
            }
            // With `whole_cache`, every body ever published under a method key:
            // an artifact never published (a lambda adapter, a probe body)
            // has no identity to re-dispatch by, and retiring it would only
            // turn its inline-cache fills away.
            if (whole_cache && body.published_key.get().is_some())
                || stale_by_record(&body, class_id, class_name)
            {
                stale.push(body);
            }
        }
        stale
    }

    /// [`stale_by_record`] for a body the cache does not hold (the VM's
    /// optimizing OSR memo, interpreter round i1 wave 23, lane L6).
    pub fn is_stale_for_redefinition(
        body: &CompiledMethod,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> bool {
        stale_by_record(body, class_id.as_u32(), class_name)
    }

    /// Mark bodies this cache never published -- the VM's optimizing OSR
    /// memo's -- as withdrawn by a redefinition
    /// ([`CompiledMethod::is_withdrawn_by_redefinition`]), without patching
    /// them: they are entered only through that memo, which the redefinition
    /// expires (its code-state epoch), so what is left is a frame already
    /// running one, which leaves at its next back-edge poll (wave 23, lane
    /// L6). Returns how many were newly marked.
    pub fn mark_withdrawn_by_redefinition(&self, bodies: &[Arc<CompiledMethod>]) -> usize {
        let mut marked = 0usize;
        for body in bodies {
            if !body
                .withdrawn_by_redefinition
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                marked += 1;
            }
        }
        if marked > 0 {
            // Cast: a count, widened.
            self.withdrawn_by_redefinition
                .fetch_add(marked as u64, std::sync::atomic::Ordering::Release);
        }
        marked
    }

    /// Force the exit polls ([`CompiledMethod::force_exit_polls`]) of every
    /// body of `bodies` a redefinition of `(class_id, class_name)` withdrew,
    /// except the bodies compiled from that class's own bytecode
    /// (interpreter round i1 wave 25, lane L6). Returns how many bodies were
    /// forced now.
    ///
    /// A frame running a forced body asks the safepoint slow path for its
    /// verdict at its next exit-capable back edge, and leaves for the
    /// interpreter there -- including a frame whose thread was inside a call
    /// during the redefinition's handshake (blocked, or the redefining thread
    /// itself), which no pause reached. See `force_exit_polls` for the why.
    ///
    /// Spared: a body of the redefined class itself is an obsolete activation
    /// the VM does not let leave (its `withdrawn_body_may_leave`: no sink
    /// resumes a frame of a redefined class), so forcing it would only make
    /// every back edge ask for a verdict that always answers "stay". Since
    /// wave 27 such a body stays spared at every later redefinition too
    /// (`CompiledMethod::exits_spared_as_obsolete`). What is forced is a body
    /// withdrawn because it spliced the class, or, on the whole-cache path,
    /// because it may have.
    ///
    /// `bodies`: what the redefinition handed [`Self::make_not_entrant`] and
    /// [`Self::mark_withdrawn_by_redefinition`], after both ran; a body not
    /// withdrawn ([`Self::body_withdrawn_by_redefinition`]: one the eviction
    /// kept published) is skipped. Under the mutation lock, as every patch of
    /// a body is, with one protection change per run of adjacent mappings.
    ///
    /// For a redefinition whose constant-pool renumbering the caller knows,
    /// see [`Self::force_withdrawn_exit_polls_after`]; this form is that one
    /// for a redefinition that moved no constant (every own-class body
    /// spared).
    pub fn force_withdrawn_exit_polls(
        &self,
        bodies: &[Arc<CompiledMethod>],
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> usize {
        self.force_withdrawn_exit_polls_after(bodies, class_id, class_name, false)
    }

    /// [`Self::force_withdrawn_exit_polls`] for a redefinition of `(class_id,
    /// class_name)` that `renumbered` the class's constant pool (some old
    /// index now names another constant: the VM's
    /// `RedefinitionHistory::last_redefinition_moved_constants`). With
    /// [`OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`] on, an OSR body compiled from
    /// the class's own bytecode is then NOT spared: it is marked
    /// ([`CompiledMethod::leaves_as_renumbered_obsolete`], before any site is
    /// written) and forced like every other withdrawn body, so its frame
    /// leaves for the interpreter at its next exit (interpreter round i1 wave
    /// 45, lane L2). A method-entry body of the class stays spared unless
    /// [`OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`] is on and a stash sink can
    /// rebuild its frame in its own bytecode (wave 46, lane L2).
    pub fn force_withdrawn_exit_polls_after(
        &self,
        bodies: &[Arc<CompiledMethod>],
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        renumbered: bool,
    ) -> usize {
        self.force_withdrawn_exit_polls_inner(bodies, class_id, class_name, renumbered, false)
    }

    /// [`Self::force_withdrawn_exit_polls`] for an interpreter-only
    /// withdrawal -- a debugger or agent request that needs `bodies`
    /// interpreted, no class redefined (interpreter round i1 wave 47, lane
    /// L2). `label` names the request in the debug lines
    /// (`<interpreter-only>`, `<breakpoint>`). No body is its own class's
    /// obsolete activation for this pass; a body an earlier redefinition
    /// spared as one ([`CompiledMethod::exits_spared_as_obsolete`]) stays
    /// spared, unless [`INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`] is on and
    /// its exit can be resumed: an OSR body, or a method-entry body
    /// [`CompiledMethod::entry_resumable_as_obsolete`] admits. Such a body
    /// is forced, and the VM's verdict decides at its next exit whether it
    /// leaves.
    pub fn force_exit_polls_for_the_interpreter(
        &self,
        bodies: &[Arc<CompiledMethod>],
        label: &str,
    ) -> usize {
        self.force_withdrawn_exit_polls_inner(
            bodies,
            cratonvm_types::ClassId::new(u32::MAX),
            label,
            false,
            true,
        )
    }

    /// The pass of [`Self::force_withdrawn_exit_polls_after`] and
    /// [`Self::force_exit_polls_for_the_interpreter`] (`for_the_interpreter`).
    fn force_withdrawn_exit_polls_inner(
        &self,
        bodies: &[Arc<CompiledMethod>],
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        renumbered: bool,
        for_the_interpreter: bool,
    ) -> usize {
        let dbg = crate::x64::dbg_jitc_enabled();
        if !FORCE_WITHDRAWN_EXIT_POLLS_ENABLED && !bodies.is_empty() {
            // Nothing is forced, so every withdrawal of this redefinition
            // leaves only at a poll it takes during a pause (wave 26).
            self.note_unforced_withdrawal();
        }
        if !FORCE_WITHDRAWN_EXIT_POLLS_ENABLED || bodies.is_empty() {
            // Wave 25 follow-up (lane L6b): the positive control prints on
            // every pass, so "never printed" can no longer mean "no candidate".
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] exit polls forced: bodies=0 sites=0 spared=0 refused=0 \
                     (candidates={}{}) redefined={class_name}",
                    bodies.len(),
                    if FORCE_WITHDRAWN_EXIT_POLLS_ENABLED {
                        ""
                    } else {
                        ", kill switch off"
                    },
                );
            }
            return 0;
        }
        let class_id = class_id.as_u32();
        let _mutation = self.mutation.lock();
        let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        let mut pending: Vec<(&Arc<CompiledMethod>, ExitPollWrites)> = Vec::new();
        let mut spared = 0usize;
        let mut refused = 0usize;
        // Wave 25 follow-up: why a candidate forced nothing, by cause.
        let mut not_withdrawn = 0usize;
        let mut no_sites = 0usize;
        let mut already = 0usize;
        for body in bodies {
            if !seen.insert(body.artifact_id) {
                continue;
            }
            let withdrawn = self.body_withdrawn_by_redefinition(body);
            let own = compiled_from_class(body, class_id, class_name);
            // Every OSR body gets a line of its own: the running loop the
            // forcing exists for is one (an optimizing OSR body from the VM's
            // memo, or a single-pass OSR body), and a pass that forced nothing
            // must say which of them it saw and why each was left alone.
            //
            // Interpreter round i2 wave 54, lane L2: and every WITHDRAWN
            // method-entry body with post-call exit sites. The positive
            // controls of the method-entry post-call sites (waves 29, 38, 39,
            // 54) name this line, which printed OSR bodies only, so a
            // method-entry body's sites could never be seen here. `chain-sites=`
            // counts the sites whose exit leaves through a caller chain (a
            // call inside a splice; waves 47 and 54).
            let osr = body.compiled_via_osr || !body.ir_osr_entries.is_empty();
            if dbg && (osr || (withdrawn && !body.post_call_exit_sites.is_empty())) {
                let chain_sites = body
                    ._deopt_point_boxes
                    .iter()
                    .filter(|p| {
                        p.reason == crate::deopt::DeoptReason::OsrExit
                            && p.frame_state.caller.is_some()
                    })
                    .count();
                eprintln!(
                    "[cratonvm-jitc] exit polls candidate: {} id={} osr={osr} ir={} \
                     withdrawn={withdrawn} own-class={own} sites={} post-call-sites={} \
                     chain-sites={chain_sites} cp-helper-sites={} forced-before={}",
                    body.method_label,
                    body.compile_id,
                    body.used_ir_backend,
                    body.exit_poll_sites.len(),
                    body.post_call_exit_sites.len(),
                    body.cp_helper_exit_sites,
                    body.exit_polls_forced
                        .load(std::sync::atomic::Ordering::Acquire),
                );
            }
            if !withdrawn {
                not_withdrawn += 1;
                continue;
            }
            // Interpreter round i1 wave 27, lane L6: spared once as its own
            // class's obsolete activation, spared for good. Its class stays
            // redefined after it was compiled, so the VM's verdict for it is
            // "stay" at every later poll (`withdrawn_body_may_leave`); a later
            // redefinition of ANOTHER class -- a whole-cache one lists every
            // live body -- used to force it anyway, and the obsolete loop then
            // paid a slow-path call per back edge for the rest of its run.
            let obsolete = body
                .exits_spared_as_obsolete
                .load(std::sync::atomic::Ordering::Acquire);
            // Interpreter round i1 wave 45, lane L2: an OSR body of the class's
            // own bytecode whose pool this redefinition renumbered leaves
            // instead of being spared (`OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`).
            // Marked before its sites are written below, so a frame that
            // reaches a forced site finds the verdict's fact already set.
            //
            // Wave 46 (lane L2): a METHOD-ENTRY body of that class too, when a
            // stash sink can rebuild its frame in the bytecode it was compiled
            // from (`OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`). Its verdict memo
            // is cleared first, so the VM asks again about this redefinition.
            let osr = body.is_osr_body();
            let leaves = own
                && renumbered
                && if osr {
                    OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE
                } else {
                    OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE && body.entry_resumable_as_obsolete()
                };
            if leaves {
                if !osr {
                    body.renumbered_entry_resumes
                        .store(0, std::sync::atomic::Ordering::Release);
                }
                body.leaves_as_renumbered_obsolete
                    .store(true, std::sync::atomic::Ordering::Release);
                if dbg {
                    eprintln!(
                        "[cratonvm-jitc] own-class {} body leaves (renumbered pool): {} id={} \
                         ir={}",
                        if osr { "OSR" } else { "method-entry" },
                        body.method_label,
                        body.compile_id,
                        body.used_ir_backend,
                    );
                }
            }
            // Interpreter round i1 wave 47, lane L2: an interpreter-only
            // request lifts the obsolete spare of a body whose exit can be
            // resumed (`INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`).
            let lifted = for_the_interpreter
                && INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE
                && obsolete
                && !own
                && (osr || body.entry_resumable_as_obsolete_for_the_interpreter());
            if lifted && dbg {
                eprintln!(
                    "[cratonvm-jitc] obsolete {} body forced for an interpreter-only request \
                     ({class_name}): {} id={} ir={}",
                    if osr { "OSR" } else { "method-entry" },
                    body.method_label,
                    body.compile_id,
                    body.used_ir_backend,
                );
            }
            if !leaves && !lifted && (own || obsolete) {
                if own {
                    body.exits_spared_as_obsolete
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                if body.has_forceable_exits() {
                    spared += 1;
                }
                continue;
            }
            if !body.has_forceable_exits() {
                no_sites += 1;
                continue;
            }
            let writes = body.exit_poll_writes();
            if dbg && matches!(&writes, Ok(w) if w.post_calls_refused) {
                // Wave 27: its frames leave at the next back edge instead.
                eprintln!(
                    "[cratonvm-jitc] post-call exits NOT forced (Shape): {}",
                    body.method_label
                );
            }
            match writes {
                Ok(writes) if writes.is_empty() => already += 1,
                Ok(writes) => pending.push((body, writes)),
                Err(why) => {
                    refused += 1;
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] exit polls NOT forced ({why:?}): {}",
                            body.method_label
                        );
                    }
                }
            }
        }
        // Cast: addresses, for the ordering and the adjacency test.
        pending.sort_by_key(|(body, _)| body._buffer.as_ptr() as usize);
        let ranges: Vec<(usize, usize)> = pending
            .iter()
            // Cast: as above.
            .map(|(body, _)| (body._buffer.as_ptr() as usize, body._buffer.mapped_len()))
            .collect();
        let mut forced = 0usize;
        let mut sites = 0usize;
        let mut pending = pending.into_iter();
        for (first, last, start, end) in adjacent_patch_runs(&ranges) {
            let run: Vec<(&Arc<CompiledMethod>, ExitPollWrites)> =
                pending.by_ref().take(last - first).collect();
            let together = run.len() > 1
                && crate::platform::make_code_patchable(start as *mut u8, end - start).is_ok();
            if together {
                for (body, writes) in run {
                    // SAFETY: prepared for `body` in this pass, under the
                    // mutation lock; its mapping lies inside `[start, end)`,
                    // which is writable now.
                    sites += unsafe { body.commit_exit_poll_writes(&writes) };
                    forced += 1;
                }
                restore_read_execute(start, end - start);
                continue;
            }
            if run.len() > 1 {
                // As in `make_not_entrant`: a refused change of the whole run
                // may have changed a part of it.
                restore_read_execute(start, end - start);
            }
            for (body, writes) in run {
                // Cast: an address, for the protection change.
                let base = body._buffer.as_ptr() as usize;
                let mapped = body._buffer.mapped_len();
                if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
                    refused += 1;
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] exit polls NOT forced (Protect): {}",
                            body.method_label
                        );
                    }
                    continue;
                }
                // SAFETY: prepared for `body` in this pass, under the mutation
                // lock, and its mapping is writable now.
                sites += unsafe { body.commit_exit_poll_writes(&writes) };
                restore_read_execute(base, mapped);
                forced += 1;
            }
        }
        if refused > 0 {
            // Wave 26 (lane L6): a frame running one of these leaves only at
            // a poll it takes during a pause, so the loop-exit handshake must
            // keep its retries (`Self::every_withdrawal_forced_since`).
            self.note_unforced_withdrawal();
        }
        if dbg {
            // Wave 28 (lane L6): and what the post-call exits forced by the
            // earlier redefinitions did since (verdicts asked / exits taken).
            let (asked, taken) = self.post_call_exit_counts();
            eprintln!(
                "[cratonvm-jitc] exit polls forced: bodies={forced} sites={sites} \
                 spared={spared} refused={refused} (candidates={} not-withdrawn={not_withdrawn} \
                 no-sites={no_sites} forced-before={already}) redefined={class_name} \
                 post-call-verdicts-so-far={asked} post-call-exits-so-far={taken}",
                bodies.len()
            );
        }
        forced
    }

    /// Remember `body` for the exit-poll pass of every later class
    /// redefinition while it lives (interpreter round i1 wave 26, lane L6):
    /// [`Self::redefinition_stale_bodies`] then reaches it after no map or
    /// baked root names it any more, and the VM lists it among the optimizing
    /// OSR bodies it withdraws ([`Self::live_unpublished_exit_poll_bodies`])
    /// after its memo forgot it. Called by [`Self::put`] / [`Self::put_osr`]
    /// for every body they publish, and by the VM for every optimizing OSR
    /// body it builds (which it never publishes), before anything can enter
    /// it. Nothing for a body with no exit poll and (wave 27) no post-call
    /// exit site: no pass can make its frame leave. One lock and a push per
    /// compiled body; a dead entry is pruned when the list's length reaches a
    /// power of two, so the list stays within twice the live population plus
    /// 64.
    pub fn note_exit_poll_body(&self, body: &Arc<CompiledMethod>) {
        // Round 16 lane chain (CH5W4-3): the same three publication funnels
        // are where the eager-chains census counts a body's chain points.
        self.note_forced_chain_publication(body);
        if !body.has_forceable_exits() {
            return;
        }
        let mut list = self.exit_poll_bodies.lock();
        let len = list.len();
        if len >= 64 && len.is_power_of_two() {
            list.retain(|known| known.strong_count() > 0);
        }
        list.push(Arc::downgrade(body));
    }

    /// Every body [`Self::note_exit_poll_body`] registered that is still
    /// alive, dead entries pruned on the way.
    fn live_exit_poll_bodies(&self) -> Vec<Arc<CompiledMethod>> {
        let mut list = self.exit_poll_bodies.lock();
        list.retain(|known| known.strong_count() > 0);
        list.iter().filter_map(std::sync::Weak::upgrade).collect()
    }

    /// The live registered bodies this cache never published: the VM's
    /// optimizing OSR bodies, including the ones its memo already forgot
    /// while a frame may still run one (interpreter round i1 wave 26, lane
    /// L6). The VM adds them to the memo's own list before a redefinition
    /// marks and forces the stale ones (`JitRealm::withdrawn_osr_memo_candidates`):
    /// before this such a body was withdrawn by a whole-cache redefinition's
    /// barrier but in no list to force, so a frame running it that was inside
    /// a call during the handshake kept its old splice until the next pause.
    pub fn live_unpublished_exit_poll_bodies(&self) -> Vec<Arc<CompiledMethod>> {
        let mut bodies = self.live_exit_poll_bodies();
        bodies.retain(|body| body.published_key.get().is_none());
        bodies
    }

    /// Record a redefinition that withdrew a body whose exit polls it could
    /// not force (interpreter round i1 wave 26, lane L6): stamped with the
    /// withdrawal count as it stands now, so
    /// [`Self::every_withdrawal_forced_since`] answers `false` for every
    /// snapshot taken before this redefinition's withdrawals.
    fn note_unforced_withdrawal(&self) {
        use std::sync::atomic::Ordering;
        let now = self
            .withdrawn_by_redefinition
            .load(Ordering::Acquire)
            .max(1);
        self.unforced_withdrawal_at.fetch_max(now, Ordering::AcqRel);
    }

    /// Were the exit polls of every body withdrawn since `before` (a
    /// [`Self::bodies_withdrawn_by_redefinition`] snapshot) forced, so that a
    /// frame running one leaves at its next exit-capable back edge without
    /// any pause (interpreter round i1 wave 26, lane L6; stage 2 of
    /// `docs/feature-designs/jit-ir-optimizations.md`)?
    /// The VM's loop-exit handshake then takes no retry pause for a peer its
    /// first pause froze: that peer leaves at its next back edge anyway.
    ///
    /// Counted as not forced: a withdrawn body with exit polls whose forcing
    /// was refused ([`ExitPollRefusal`]), and every withdrawal while
    /// [`FORCE_WITHDRAWN_EXIT_POLLS_ENABLED`] is off. Not counted, because no
    /// pause makes their frames leave either: a body with no exit poll, and a
    /// body of the redefined class itself (spared: its obsolete activation
    /// stays). A whole-cache redefinition's barrier withdraws bodies no scan
    /// lists, but every body that has exit polls is registered
    /// ([`Self::note_exit_poll_body`]) and listed by the scan or by the VM.
    ///
    /// Conservative at the edges: an unforced withdrawal recorded at exactly
    /// the snapshot's count (one made by the redefinition just before it)
    /// answers `false`, which only keeps the retries.
    pub fn every_withdrawal_forced_since(&self, before: u64) -> bool {
        let at = self
            .unforced_withdrawal_at
            .load(std::sync::atomic::Ordering::Acquire);
        at == 0 || at < before
    }

    /// Count one verdict a forced post-call exit site asked for (the VM's
    /// safepoint slow path in `POST_CALL_EXIT_VERDICT_ONLY` mode), `left`
    /// when it sent the frame out; returns this verdict's ordinal, from 1
    /// (interpreter round i1 wave 28, lane L6). The positive control of the
    /// wave-27 post-call exits: before it nothing showed whether a forced site
    /// was ever reached. Two relaxed adds on a path that runs only after a
    /// redefinition forced the site.
    pub fn note_post_call_exit_verdict(&self, left: bool) -> u64 {
        use std::sync::atomic::Ordering;
        if left {
            self.post_call_exits_taken.fetch_add(1, Ordering::Relaxed);
        }
        self.post_call_exit_verdicts.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `(verdicts asked, exits taken)` so far
    /// ([`Self::note_post_call_exit_verdict`]).
    fn post_call_exit_counts(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.post_call_exit_verdicts.load(Ordering::Relaxed),
            self.post_call_exits_taken.load(Ordering::Relaxed),
        )
    }

    /// JIT round 23 wave 1, lane L1 (THROWS22-1, the OSR profit exit;
    /// `CRATONVM_JIT_OSR_PROFIT_EXIT`, default ON): arm the SINGLE-PASS OSR
    /// body `body` to leave for the interpreter at its next back-edge poll,
    /// because the VM now holds an enterable optimizing OSR body for every
    /// loop header of its method (the VM asks that before calling;
    /// `jit_bridge::arm_single_pass_osr_profit_exit`).
    ///
    /// The fact is set first ([`CompiledMethod::osr_profit_exit_armed`]),
    /// then every one of the body's exit poll sites is forced
    /// ([`forced_exit_poll_cc`]), under `mutation` like every other patch of
    /// this cache, so a frame that reaches a forced poll finds the fact set
    /// and the VM's poll verdict ([`Self::osr_profit_exit_may_leave`]) sends
    /// it out through the poll's `OsrExit` map -- the exit a JVMTI
    /// interpreter-only request takes. Only the polls: a post-call exit site
    /// would leave mid-iteration, and the next back edge is at most one
    /// iteration away. `exit_polls_forced` is left alone, so a later
    /// redefinition still forces the post-call sites.
    ///
    /// `Ok(sites written)` (`0` when armed already, or when a redefinition
    /// forced the polls before: the fact alone then suffices), or why the
    /// body was not armed. Nothing is armed and nothing written on a refusal.
    /// A body a redefinition withdrew or spared as its class's obsolete
    /// activation is refused: its exits belong to the redefinition's verdict.
    pub fn arm_osr_profit_exit(&self, body: &Arc<CompiledMethod>) -> Result<usize, &'static str> {
        use std::sync::atomic::{AtomicU8, Ordering};
        if !self.osr_profit_exit {
            return Err("switch-off");
        }
        // An OPTIMIZING OSR body is armed too (`late_bind_osr_exit`: a frame in
        // a body a late bind superseded leaves for the rebuilt one); it leaves
        // through its poll mode exits, which `exit_poll_sites` below requires.
        if !(body.compiled_via_osr || !body.ir_osr_entries.is_empty()) {
            return Err("not-an-osr-body");
        }
        if body.exit_poll_sites.is_empty() {
            return Err("no-exit-polls");
        }
        if !cfg!(target_arch = "x86_64") {
            return Err("unsupported");
        }
        let _mutation = self.mutation.lock();
        if self.body_withdrawn_by_redefinition(body)
            || body.exits_spared_as_obsolete.load(Ordering::Acquire)
            || body.leaves_as_renumbered_obsolete()
        {
            return Err("redefinition-owns-its-exits");
        }
        if body.osr_profit_exit_armed.load(Ordering::Acquire) {
            return Ok(0);
        }
        if body.osr_profit_exit_disarmed.load(Ordering::Acquire) {
            return Err("disarmed-before");
        }
        let code = body._buffer.as_slice();
        // Cast: an address, for the displacement check and the writes.
        let base = code.as_ptr() as usize;
        let mut polls: Vec<(usize, u8)> = Vec::with_capacity(body.exit_poll_sites.len());
        for &site in &body.exit_poll_sites {
            // Cast: a buffer offset, widened.
            let cc = site as usize;
            let Some(byte) = forced_exit_poll_cc(code, base, cc, body.exit_poll_flag) else {
                return Err("shape");
            };
            polls.push((base + cc, byte));
        }
        if body.exit_polls_forced.load(Ordering::Acquire) {
            body.osr_profit_exit_armed.store(true, Ordering::Release);
            self.osr_profit_armed.fetch_add(1, Ordering::Relaxed);
            return Ok(0);
        }
        let mapped = body._buffer.mapped_len();
        if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
            return Err("protect");
        }
        // Before the first write: a frame reaching a forced poll must find
        // the fact its verdict asks.
        body.osr_profit_exit_armed.store(true, Ordering::Release);
        self.osr_profit_armed.fetch_add(1, Ordering::Relaxed);
        for &(addr, byte) in &polls {
            // SAFETY: `addr` is a checked condition-code byte inside this
            // body's emitted code, writable now; one byte store, so a thread
            // fetching the instruction sees the old or the new branch whole,
            // and both are correct code (`forced_exit_poll_cc`).
            unsafe {
                (*(addr as *const AtomicU8)).store(byte, Ordering::SeqCst);
            }
        }
        restore_read_execute(base, mapped);
        Ok(polls.len())
    }

    /// The live SINGLE-PASS OSR bodies of `current`'s method that a later
    /// `put_osr` superseded in the OSR map (t1 lane jitea, L1W1-2 of
    /// `docs/internal/proposals/jit/tier1/jit-r23-w1-L1-proposals.md`): a
    /// de-spec-stale or late-bind recompile replaced them, but a frame that
    /// entered one before still runs it (the frame's door holds its `Arc`, so
    /// it is alive in [`Self::live_exit_poll_bodies`]). Same owner class and
    /// method label (class, name and descriptor) as `current`, not `current`
    /// itself, not armed or disarmed already. Empty for an IR body.
    ///
    /// The VM arms each through [`Self::arm_osr_profit_exit`] when it arms
    /// `current` (`jit_bridge::arm_single_pass_osr_profit_exit`, under
    /// `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED`), which refuses a body whose
    /// exits a redefinition owns. One lock of the registration list.
    pub fn superseded_single_pass_osr_bodies(
        &self,
        current: &Arc<CompiledMethod>,
    ) -> Vec<Arc<CompiledMethod>> {
        if !current.compiled_via_osr || current.used_ir_backend {
            return Vec::new();
        }
        let mut bodies = self.live_exit_poll_bodies();
        bodies.retain(|body| superseded_single_pass_osr_sibling(current, body));
        bodies
    }

    /// Take back the arm of `body` (item 1 of
    /// `docs/feature-designs/deopt-osr.md`): a
    /// profit verdict for it could not send its frame out (its poll could not
    /// be named, or a chain sink refused), so without this the frame would pay
    /// a slow-path call at every back edge for the rest of its activation.
    /// Under `mutation`, like every patch of this cache: the fact is cleared
    /// and the body marked never to be armed again, then each forced poll is
    /// written back (`JO` -> `JZ`, `JNO` -> `JNZ`) -- unless a redefinition
    /// forced the polls meanwhile (`exit_polls_forced`), whose force must
    /// stand. Returns how many sites were restored; `0` when the body was not
    /// armed (a racing disarm won) or the polls stay forced.
    pub fn disarm_osr_profit_exit(&self, body: &CompiledMethod) -> usize {
        use std::sync::atomic::{AtomicU8, Ordering};
        let _mutation = self.mutation.lock();
        if !body.osr_profit_exit_armed.swap(false, Ordering::AcqRel) {
            return 0;
        }
        body.osr_profit_exit_disarmed.store(true, Ordering::Release);
        self.osr_profit_disarms.fetch_add(1, Ordering::Relaxed);
        if body.exit_polls_forced.load(Ordering::Acquire) || !cfg!(target_arch = "x86_64") {
            return 0;
        }
        let code = body._buffer.as_slice();
        // Cast: an address, for the displacement check and the writes.
        let base = code.as_ptr() as usize;
        let mut restores: Vec<(usize, u8)> = Vec::with_capacity(body.exit_poll_sites.len());
        for &site in &body.exit_poll_sites {
            // Cast: a buffer offset, widened.
            let cc = site as usize;
            // Only a site this pass can still recognise as a forced poll.
            if forced_exit_poll_cc(code, base, cc, body.exit_poll_flag).is_some()
                && matches!(code[cc], 0x80 | 0x81)
            {
                restores.push((base + cc, code[cc] | 0x04));
            }
        }
        if restores.is_empty() {
            return 0;
        }
        let mapped = body._buffer.mapped_len();
        if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
            // The polls stay forced; the verdict says "stay" for a disarmed body.
            return 0;
        }
        for &(addr, byte) in &restores {
            // SAFETY: a checked condition-code byte of this body's emitted
            // code, writable now; one byte store, both branches correct code.
            unsafe {
                (*(addr as *const AtomicU8)).store(byte, Ordering::SeqCst);
            }
        }
        restore_read_execute(base, mapped);
        restores.len()
    }

    /// Armed bodies [`Self::disarm_osr_profit_exit`] took back so far.
    pub fn osr_profit_exit_disarms(&self) -> u64 {
        self.osr_profit_disarms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// May the polling body `body` be told to leave for the interpreter for
    /// PROFIT ([`Self::arm_osr_profit_exit`])? Armed, still a single-pass
    /// OSR body, and not one whose exits a class redefinition owns (withdrawn,
    /// spared as the obsolete activation, or leaving as the renumbered one):
    /// those keep the redefinition's verdict, whatever it is. Asked by the
    /// VM's safepoint slow path only while [`Self::osr_profit_exits_armed`]
    /// is non-zero.
    pub fn osr_profit_exit_may_leave(&self, body: &CompiledMethod) -> bool {
        use std::sync::atomic::Ordering;
        body.osr_profit_exit_armed.load(Ordering::Acquire)
            && (body.compiled_via_osr || !body.ir_osr_entries.is_empty())
            && !self.body_withdrawn_by_redefinition(body)
            && !body.exits_spared_as_obsolete.load(Ordering::Acquire)
            && !body.leaves_as_renumbered_obsolete()
    }

    /// `CRATONVM_JIT_OSR_PROFIT_EXIT` as this VM read it at construction
    /// (JIT round 23 wave 1, lane L1): the VM's arming pass asks it before it
    /// looks for headers.
    #[inline]
    pub fn osr_profit_exit_enabled(&self) -> bool {
        self.osr_profit_exit
    }

    /// How many bodies [`Self::arm_osr_profit_exit`] has armed in this cache;
    /// the safepoint slow path's one-load gate for the profit verdict.
    #[inline]
    pub fn osr_profit_exits_armed(&self) -> u64 {
        self.osr_profit_armed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Count one profit verdict that told a frame to leave; returns its
    /// ordinal, from 1.
    pub fn note_osr_profit_exit_verdict(&self) -> u64 {
        self.osr_profit_verdicts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Count one profit exit the OSR door's transfer sink took (committed);
    /// returns its ordinal, from 1.
    pub fn note_osr_profit_exit_taken(&self) -> u64 {
        self.osr_profit_exits_taken
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// `(armed, verdicts, taken)`: the OSR profit exit's DBG counters
    /// (JIT round 23 wave 1, lane L1). `verdicts` above `taken` means a
    /// frame was told to leave and its exit was not committed (a refused
    /// transfer); `armed` with no verdicts means no armed body polled again.
    pub fn osr_profit_exit_counts(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.osr_profit_armed.load(Ordering::Relaxed),
            self.osr_profit_verdicts.load(Ordering::Relaxed),
            self.osr_profit_exits_taken.load(Ordering::Relaxed),
        )
    }

    /// How many bodies of this cache a redefinition has withdrawn so far
    /// ([`CompiledMethod::is_withdrawn_by_redefinition`]), plus one per
    /// whole-cache redefinition ([`Self::note_whole_cache_redefinition`],
    /// which withdraws bodies nobody can count). Zero until the first such
    /// redefinition, so the VM's safepoint slow path can skip looking up the
    /// polling body while it is (wave 23, lane L6); it moves on every
    /// redefinition that withdrew anything, which is what the VM's
    /// redefinition path compares to decide on its loop-exit handshake.
    pub fn bodies_withdrawn_by_redefinition(&self) -> u64 {
        self.withdrawn_by_redefinition
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record a redefinition that flushed this whole cache (interpreter round
    /// i1 wave 24, lane L6): every body whose compilation began before now is
    /// withdrawn by it, whatever it copied -- the whole-cache path is taken
    /// exactly because some copy left no record
    /// (`JitRealm::scoped_redefinition_admitted`). That includes bodies no
    /// scan can reach: an optimizing OSR body the VM's memo had already
    /// forgotten while a frame still runs it, and a body published between the
    /// redefinition's candidate scan and its flush.
    /// [`Self::body_withdrawn_by_redefinition`] answers for them.
    ///
    /// Called right after the redefinition's flush ([`Self::clear_all`]), whose
    /// install barrier it takes: strictly above the stamp of every compilation
    /// that had begun, and at or below the stamp of every compilation that
    /// began after it -- which, under the class-manager writer the
    /// redefinition holds, reads the new bytecode. A barrier of its own, taken
    /// later, would also withdraw those: an OSR body compiled in between would
    /// be declined by the OSR door for good.
    pub fn note_whole_cache_redefinition(&self) {
        use std::sync::atomic::Ordering;
        let barrier = self.flush_barrier.load(Ordering::Acquire);
        self.whole_cache_redefinition_barrier
            .fetch_max(barrier, Ordering::AcqRel);
        self.withdrawn_by_redefinition
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Was `body` withdrawn by a class redefinition of this cache's VM: marked
    /// ([`CompiledMethod::is_withdrawn_by_redefinition`]), or compiled before
    /// the last whole-cache redefinition ([`Self::note_whole_cache_redefinition`])?
    /// A body the barrier answers for is marked now, so the VM's exit sinks,
    /// which read the mark, agree with the verdict that sent it out
    /// (interpreter round i1 wave 24, lane L6).
    ///
    /// Asked by the VM's safepoint slow path about the body whose poll it
    /// serves, which may be one the cache never held.
    pub fn body_withdrawn_by_redefinition(&self, body: &CompiledMethod) -> bool {
        use std::sync::atomic::Ordering;
        if body.is_withdrawn_by_redefinition() {
            return true;
        }
        let barrier = self
            .whole_cache_redefinition_barrier
            .load(Ordering::Acquire);
        if barrier == 0 || body.install_epoch >= barrier {
            return false;
        }
        // Not counted: the whole-cache redefinition already moved the count
        // for every body it withdrew.
        body.withdrawn_by_redefinition
            .store(true, Ordering::Release);
        true
    }

    /// Make every body of `bodies` that is no longer published not entrant
    /// ([`CompiledMethod::make_not_entrant`]), and mark it retired, so no
    /// inline cache, cell or baked caller published later takes it. Returns
    /// how many were patched now. Nothing happens until the cache is armed
    /// ([`Self::arm_not_entrant`]).
    ///
    /// A body still published is skipped: the cache's own lookups would hand
    /// it out again and the re-dispatch would find it once more. The
    /// redefinition's eviction withdraws every stale body before this runs
    /// (the scoped eviction's closure, or the full flush); a published one
    /// here is a body the eviction deliberately kept.
    pub fn make_not_entrant(&self, bodies: &[Arc<CompiledMethod>]) -> usize {
        self.make_not_entrant_as(bodies, NotEntrantCause::Redefinition)
    }

    /// [`Self::make_not_entrant`] for bodies a cache EVICTION withdrew (a
    /// deoptimization's `remove`, a displaced late-bind body) rather than a
    /// class redefinition (JIT round 16 wave 15, lane trapfix;
    /// `r16-framestate-spliced-callee-trap-repeats-after-make-not-compilable-FIXED-20261002.md`).
    ///
    /// The patch, the forward word and the counters are the same. What is not
    /// done is the redefinition's bookkeeping: the body is not marked
    /// [`CompiledMethod::is_withdrawn_by_redefinition`] (its bytecode is still
    /// the class's current bytecode, so a frame running it may finish in it
    /// and the OSR doors need not forget it as obsolete), the cache's
    /// redefinition count does not move, and a refused patch is not recorded
    /// as an unpatched redefinition body ([`Self::min_unpatched_cp_stamp`]:
    /// its constant-pool indices are current ones).
    pub fn make_not_entrant_after_eviction(&self, bodies: &[Arc<CompiledMethod>]) -> usize {
        self.make_not_entrant_as(bodies, NotEntrantCause::Eviction)
    }

    /// The pass behind [`Self::make_not_entrant`] ([`NotEntrantCause::Redefinition`])
    /// and [`Self::make_not_entrant_after_eviction`] ([`NotEntrantCause::Eviction`]).
    fn make_not_entrant_as(&self, bodies: &[Arc<CompiledMethod>], cause: NotEntrantCause) -> usize {
        let by_redefinition = cause.retires_old_bytecode();
        let Some(hook) = *self.not_entrant_hook.lock() else {
            return 0;
        };
        if bodies.is_empty() {
            return 0;
        }
        let _mutation = self.mutation.lock();
        let mut published: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    published.insert(cm.artifact_id);
                }
            }
        }
        let dbg = crate::x64::dbg_jitc_enabled();
        let mut patched = 0usize;
        // This pass's refusals by slot, for the summary line.
        let mut refused = [0u64; NOT_ENTRANT_REFUSAL_SLOTS];
        let mut withdrawn = 0u64;
        let refuse = |body: &Arc<CompiledMethod>, why: NotEntrantRefusal, refused: &mut [u64]| {
            // Wave 25 (lane L6): retired but still enterable by a baked
            // caller, with its old constant-pool indices. Only a redefinition
            // leaves old indices behind (round 16 wave 15, lane trapfix).
            if by_redefinition {
                self.note_unpatched(body);
            }
            self.not_entrant_refused
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.not_entrant_refusals[why.index()]
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            refused[why.index()] += 1;
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] not-entrant REFUSED ({why:?}): {} entry={:#x}",
                    body.method_label,
                    // Cast: an address, for the trace line.
                    body.entry_ptr() as usize,
                );
            }
        };
        let note_patched = |body: &CompiledMethod| {
            self.not_entrant_patched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] not-entrant: {} entry={:#x}",
                    body.method_label,
                    // Cast: an address, for the trace line.
                    body.entry_ptr() as usize,
                );
            }
        };
        // First every check, and every patch prepared but not written
        // (interpreter round i1 wave 24, lane L6): then the mappings can be
        // made writable a run of adjacent ones at a time, not two protection
        // changes (and a TLB shootdown) per body -- a whole-cache redefinition
        // patches every body of the cache.
        let mut prepared: Vec<(&Arc<CompiledMethod>, PreparedNotEntrantPatch)> = Vec::new();
        let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
        for body in bodies {
            if published.contains(&body.artifact_id) {
                self.not_entrant_refused
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.not_entrant_refusals[NOT_ENTRANT_REFUSED_PUBLISHED]
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                refused[NOT_ENTRANT_REFUSED_PUBLISHED] += 1;
                continue;
            }
            body.retired
                .store(true, std::sync::atomic::Ordering::Release);
            // Whether or not its entry can be patched below: a frame running
            // it may leave at its next back-edge poll (wave 23). Not for an
            // eviction's body, which runs current bytecode.
            if by_redefinition
                && !body
                    .withdrawn_by_redefinition
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                withdrawn += 1;
            }
            // Patched by an earlier pass, or listed twice in this one: two
            // prepared patches of one body would each bake their own record.
            if body.is_not_entrant() || !seen.insert(body.artifact_id) {
                continue;
            }
            match body
                .not_entrant_record(hook)
                .and_then(|record| body.prepare_not_entrant_patch(record, hook.helper))
            {
                Ok(patch) => prepared.push((body, patch)),
                Err(why) => refuse(body, why, &mut refused[..]),
            }
        }
        prepared.sort_by_key(|(_, patch)| patch.base);
        let ranges: Vec<(usize, usize)> = prepared
            .iter()
            .map(|(_, patch)| (patch.base, patch.mapped))
            .collect();
        let mut prepared = prepared.into_iter();
        for (first, last, start, end) in adjacent_patch_runs(&ranges) {
            let run: Vec<(&Arc<CompiledMethod>, PreparedNotEntrantPatch)> =
                prepared.by_ref().take(last - first).collect();
            let together = run.len() > 1
                && crate::platform::make_code_patchable(start as *mut u8, end - start).is_ok();
            if together {
                for (body, patch) in run {
                    // SAFETY: prepared for `body` in this pass, under the
                    // mutation lock; its mapping lies inside `[start, end)`,
                    // which is writable now.
                    unsafe { body.commit_not_entrant_patch(patch) };
                    patched += 1;
                    note_patched(&**body);
                }
                restore_read_execute(start, end - start);
                continue;
            }
            if run.len() > 1 {
                // A refused change of the whole run may still have changed a
                // part of it: back to read-execute first, then one at a time.
                restore_read_execute(start, end - start);
            }
            for (body, patch) in run {
                let (base, mapped) = (patch.base, patch.mapped);
                if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
                    refuse(body, NotEntrantRefusal::Protect, &mut refused[..]);
                    continue;
                }
                // SAFETY: prepared for `body` in this pass, under the mutation
                // lock, and its mapping is writable now.
                unsafe { body.commit_not_entrant_patch(patch) };
                restore_read_execute(base, mapped);
                patched += 1;
                note_patched(&**body);
            }
        }
        if withdrawn > 0 {
            self.withdrawn_by_redefinition
                .fetch_add(withdrawn, std::sync::atomic::Ordering::Release);
        }
        if dbg {
            // One line per pass: what item 3 of the wave-23 L6 brief asks for
            // on a real workload (how many bodies each refusal leaves as they
            // were), with the cache's running totals.
            let mut reasons = String::new();
            for kind in NotEntrantRefusal::ALL {
                reasons.push_str(&format!(" {kind:?}={}", refused[kind.index()]));
            }
            let (total_patched, total_refused) = self.not_entrant_counts();
            eprintln!(
                "[cratonvm-jitc] not-entrant pass: candidates={} patched={patched} \
                 Published={}{reasons} (cache totals: patched={total_patched} \
                 refused={total_refused}) by={}",
                bodies.len(),
                refused[NOT_ENTRANT_REFUSED_PUBLISHED],
                cause.label(),
            );
        }
        patched
    }

    /// Record `body` as retired by a redefinition but left enterable (its
    /// not-entrant patch was refused), for [`Self::min_unpatched_cp_stamp`].
    /// Once per body; dropped bodies are forgotten on the way.
    fn note_unpatched(&self, body: &Arc<CompiledMethod>) {
        let weak = Arc::downgrade(body);
        let mut list = self.unpatched_bodies.lock();
        if list.iter().any(|(known, _)| known.ptr_eq(&weak)) {
            return;
        }
        list.retain(|(known, _)| known.strong_count() > 0);
        list.push((weak, body.compile_cp_stamp.unwrap_or(0)));
    }

    /// The least constant-pool stamp of a live body a redefinition retired
    /// but could not make not entrant, `None` when there is none
    /// (interpreter round i1 wave 25, lane L6;
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    ///
    /// A baked caller may still enter such a body, and its `ldc` / `new` /
    /// type-check sites translate their indices through the class's
    /// redefinition history from that stamp on
    /// (`vm::jit::helpers::stale_cp_site_index`). The stale-frame census does
    /// not see a body that has not started yet, so the VM lowers its prune
    /// floor to this (`obsolete_frames::prune_histories_by_census`): the
    /// history keeps the steps such a body needs for as long as it can run.
    pub fn min_unpatched_cp_stamp(&self) -> Option<u64> {
        let mut list = self.unpatched_bodies.lock();
        list.retain(|(known, _)| known.strong_count() > 0);
        list.iter().map(|&(_, stamp)| stamp).min()
    }

    /// `(patched, refused)`: bodies this cache made not entrant, and bodies
    /// it was asked to but left as they were (still published, or a
    /// [`NotEntrantRefusal`]).
    pub fn not_entrant_counts(&self) -> (u64, u64) {
        (
            self.not_entrant_patched
                .load(std::sync::atomic::Ordering::Relaxed),
            self.not_entrant_refused
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Why a not-entrant pass runs (j27 wave 4, lane DEOPTROWS, TF15-4): a class
/// REDEFINITION retired the bodies (old bytecode, old constant-pool indices),
/// or a cache EVICTION withdrew bodies that still run current bytecode (a
/// deoptimization's `remove`, a displaced late-bind body). Replaces the
/// `by_redefinition: bool` the pass took, and names itself on the pass's
/// `CRATONVM_DBG_JITC` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotEntrantCause {
    Redefinition,
    Eviction,
}

impl NotEntrantCause {
    /// Does this cause leave old bytecode behind: mark the bodies withdrawn
    /// by redefinition, and record a refused patch as an unpatched
    /// redefinition body ([`JitCache::min_unpatched_cp_stamp`])?
    fn retires_old_bytecode(self) -> bool {
        self == Self::Redefinition
    }

    /// The `by=` word of the pass line.
    fn label(self) -> &'static str {
        match self {
            Self::Redefinition => "redefinition",
            Self::Eviction => "eviction",
        }
    }
}

#[cfg(test)]
mod j27_tf15_4_not_entrant_cause_tests {
    use super::NotEntrantCause;

    /// Only a redefinition leaves old bytecode behind; each cause names
    /// itself distinctly on the pass line.
    #[test]
    fn only_a_redefinition_retires_old_bytecode_and_each_cause_is_named() {
        assert!(NotEntrantCause::Redefinition.retires_old_bytecode());
        assert!(!NotEntrantCause::Eviction.retires_old_bytecode());
        assert_eq!(NotEntrantCause::Redefinition.label(), "redefinition");
        assert_eq!(NotEntrantCause::Eviction.label(), "eviction");
    }
}

/// nio21 wave 16 (lane L1): should [`JitCache::put`] make `old`, which the
/// publication of `new` under the same key just displaced, not entrant with
/// its stub forwarding to `new`?
///
/// Every pair [`supersede_forward_admissible`] admits (a stub that can
/// forward; anything else would send each baked call through the VM's by-name
/// re-dispatch, slower than the old body), except an optimizing body displaced
/// by a single-pass one: its baked callers keep the faster code they have. A
/// single-pass body displaced by another single-pass body IS redirected: the
/// second compile of a method already at C1 (an eager or callee door, then the
/// manager's own request) left every caller compiled in between on the first
/// body for life -- and the entry counter the drain promotes by is read only
/// from the PUBLISHED body, so those callers' calls were never counted and the
/// method never reached C2 (`r21-tier-treemap-insert-path-stays-single-pass`,
/// `TreeMap.addEntry` / `fixAfterInsertion`).
pub fn supersede_redirect_wanted(old: &CompiledMethod, new: &CompiledMethod) -> bool {
    supersede_forward_admissible(old, new) && !(old.used_ir_backend && !new.used_ir_backend)
}

/// `CRATONVM_JIT_SUPERSEDE_SKIP_REFUSED_FORWARD` (STASH20-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`, JIT round 20 wave
/// 19, lane yStash; **default ON**, `=0` kills): a displaced body whose method
/// is a sticky recursion-cycle member is NOT made not entrant
/// ([`JitCache::redirect_superseded_locked`]). The VM's
/// `jit_not_entrant_entry` refuses such a body's forward (a statically bound
/// call into the method must keep the dispatch helper's depth guard), so a
/// redirect turned every call its baked callers make into a by-name VM
/// re-dispatch (microseconds) where the displaced body ran in nanoseconds.
/// Left entrant, the baked callers keep the edge they baked, which points at
/// an artifact older than themselves (the acyclic-baked-edge argument the
/// cycle verdict rests on still holds; a forward to the NEWER body is what
/// the VM refuses). The other refusal, an agent that may need methods
/// interpreted, is not skipped: there the by-name route is the one that asks.
/// Read once per publication that displaces a body (cold).
pub fn supersede_skip_refused_forward_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SUPERSEDE_SKIP_REFUSED_FORWARD")
}

/// Do two bodies name the same published method (class, name, descriptor and
/// declaring class id)? A pending body is filed by key HASH only.
fn same_published_key(a: &CompiledMethod, b: &CompiledMethod) -> bool {
    match (a.published_key.get(), b.published_key.get()) {
        (Some(ka), Some(kb)) => ka.0 == kb.0 && ka.1 == kb.1 && ka.2 == kb.2 && ka.3 == kb.3,
        _ => false,
    }
}

/// How many displaced bodies one key may wait with while the cache is
/// unarmed; past it the oldest is forgotten (it keeps running for its baked
/// callers, the pre-wave-16 behaviour).
const SUPERSEDE_PENDING_PER_KEY: usize = 8;

impl JitCache {
    /// nio21 wave 16 (lane L1; W15L3-1 of `jit-r21-nio-proposals-w15-L3.md`):
    /// the publication of `new` under the key hashing to `key_hash` displaced
    /// `displaced` (if any). Make it not entrant, its stub forwarding to the
    /// method's current body, so every caller that baked its entry -- a
    /// direct bind, a splice's call, an OSR body, a Rust door holding the raw
    /// entry -- reaches `new` at its next call through one patched `JMP` and
    /// the stub's forwarding prefix (after one first call through the VM's
    /// `jit_not_entrant_entry`, which installs the forward under the VM's own
    /// policy: no forward while an agent may need methods interpreted, nor to
    /// a method a direct call must dispatch). HotSpot makes the old nmethod
    /// not entrant on every replacement for the same reason.
    ///
    /// Frames already running the displaced body finish in it (only NEW
    /// entries are redirected; the body stays mapped for as long as anything
    /// roots it). OSR bodies are never displaced here (`put_osr`).
    ///
    /// Not armed yet ([`Self::arm_not_entrant`]): the displaced body waits,
    /// weakly, for the next publication of the key once armed. Called by
    /// [`Self::put`] with `mutation` held, after the swap: `displaced` is in
    /// no map. Returns how many bodies were patched.
    pub(crate) fn redirect_superseded_locked(
        &self,
        key_hash: u64,
        displaced: Option<&Arc<CompiledMethod>>,
        new: &Arc<CompiledMethod>,
    ) -> usize {
        use std::sync::atomic::Ordering;
        if !self.supersede_redirect {
            return 0;
        }
        let hook = *self.not_entrant_hook.lock();
        // STASH20-1: a body whose forward the VM would refuse stays entrant.
        let skip_refused = supersede_skip_refused_forward_enabled();
        let forward_refused = |body: &CompiledMethod| -> bool {
            let refused = skip_refused && self.displaced_body_must_dispatch(body);
            if refused && crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] supersede-redirect SKIPPED (the VM would refuse its forward: \
                     a direct call must dispatch this method): {}",
                    body.method_label,
                );
            }
            refused
        };
        let mut bodies: Vec<Arc<CompiledMethod>> = Vec::new();
        if let Some(old) = displaced {
            if supersede_redirect_wanted(old, new) && !forward_refused(old.as_ref()) {
                if hook.is_some() {
                    bodies.push(Arc::clone(old));
                } else {
                    let mut pending = self.supersede_pending.lock();
                    if pending.len() >= 64 && pending.len().is_power_of_two() {
                        pending.retain(|_, list| {
                            list.retain(|w| w.strong_count() > 0);
                            !list.is_empty()
                        });
                    }
                    let list = pending.entry(key_hash).or_default();
                    list.retain(|w| w.strong_count() > 0);
                    if list.len() >= SUPERSEDE_PENDING_PER_KEY {
                        list.remove(0);
                    }
                    list.push(Arc::downgrade(old));
                }
            }
        }
        let Some(hook) = hook else {
            return 0;
        };
        let waiting = {
            let mut pending = self.supersede_pending.lock();
            if pending.is_empty() {
                None
            } else {
                pending.remove(&key_hash)
            }
        };
        for weak in waiting.into_iter().flatten() {
            let Some(body) = weak.upgrade() else {
                continue;
            };
            if same_published_key(&body, new)
                && supersede_redirect_wanted(&body, new)
                && !forward_refused(body.as_ref())
            {
                bodies.push(body);
            } else {
                // Released through the queue: the upgrade may be the last
                // owner of a body a thread is still inside.
                drop(crate::RetainedCode::new(body));
            }
        }
        let dbg = crate::x64::dbg_jitc_enabled();
        let mut patched = 0usize;
        for body in &bodies {
            match body.make_not_entrant(hook) {
                Ok(()) => {
                    // As the eviction pass marks its bodies: no inline cache,
                    // cell or memo published later takes it.
                    body.retired.store(true, Ordering::Release);
                    patched += 1;
                    self.supersede_redirects[0].fetch_add(1, Ordering::Relaxed);
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] supersede-redirect: {} {} -> {} (entry={:#x})",
                            body.method_label,
                            if body.used_ir_backend { "C2" } else { "C1" },
                            if new.used_ir_backend { "C2" } else { "C1" },
                            // Cast: an address, for the trace line.
                            body.entry_ptr() as usize,
                        );
                    }
                }
                Err(why) => {
                    self.supersede_redirects[1].fetch_add(1, Ordering::Relaxed);
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] supersede-redirect REFUSED ({why:?}): {}",
                            body.method_label,
                        );
                    }
                }
            }
        }
        // Each through the queue: a pending body's upgrade may be its last
        // owner (the displaced one is still owned by `put`'s `superseded`).
        for body in bodies {
            drop(crate::RetainedCode::new(body));
        }
        patched
    }

    /// `(patched, refused)` by the supersede redirect
    /// (`JitCache::redirect_superseded_locked`) so far.
    pub fn supersede_redirect_counts(&self) -> (u64, u64) {
        (
            self.supersede_redirects[0].load(std::sync::atomic::Ordering::Relaxed),
            self.supersede_redirects[1].load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// STASH20-1 (JIT round 20 wave 19, lane yStash): would the VM refuse
    /// the forward of `body`'s not-entrant stub because its method is a
    /// sticky recursion-cycle member (a statically bound call into it must
    /// keep the dispatch helper and its depth guard:
    /// `JitVerdictRegistry::direct_call_requires_dispatch_of`, the verdict
    /// `jit_not_entrant_entry` asks)? Asked through the VM's tiered manager,
    /// the cache's withdrawal sink ([`Self::install_withdrawal_sink`]);
    /// `false` when no sink is installed or the body names no method.
    fn displaced_body_must_dispatch(&self, body: &CompiledMethod) -> bool {
        let Some((class_name, method_name, descriptor, class_id)) = body.published_key.get() else {
            return false;
        };
        let Some(sink) = self
            .withdrawal_sink
            .get()
            .and_then(std::sync::Weak::upgrade)
        else {
            return false;
        };
        sink.forward_must_dispatch(class_name, method_name, descriptor, *class_id)
    }

    /// STASH20-2 (JIT round 20 wave 19, lane yStash; HotSpot's
    /// `SharedRuntime::handle_wrong_method` re-resolving the caller's call
    /// site): a call returning to `ret_pc` in `caller` has just come through
    /// the not-entrant stub of `record`'s body, whose stub the VM pointed at
    /// `target`, the method's current body. Re-point that call site at
    /// `target`, so the caller's later calls skip the patched `JMP`, the
    /// forwarding prefix and the indirect `JMP`.
    ///
    /// Re-pointed only when every one of these holds; otherwise nothing is
    /// written and the stub keeps forwarding (the reason is returned):
    ///
    /// * **The site is the caller's direct `CALL rel32` of the patched body.**
    ///   The byte at `ret_pc - 5` is `E8` and its displacement resolves to the
    ///   patched body's entry (the decode the stack walker trusts for a
    ///   frame's identity), the instruction lies in the caller's emitted code,
    ///   and its last two bytes are not a register-indirect `CALL` (the
    ///   inline cache's `CALL R11`), whose ModRM would read as a displacement.
    /// * **Lifetime.** The caller ROOTS the patched body
    ///   (`_direct_callee_roots`, by artifact), and a non-cycle forward of the
    ///   record owns `target` ([`NotEntrantRecord::rebind_refusal`]): the
    ///   caller owns the patched body, which owns `target` through a box that
    ///   is never released, so the rebound site can never reach freed code.
    ///   No later forward of the record may close a cycle
    ///   ([`NotEntrantRecord::note_call_site_rebound`]).
    /// * **Atomicity.** The four displacement bytes lie in one 8-aligned word
    ///   inside the caller's mapping, rewritten by ONE compare-exchange that
    ///   keeps the word's other bytes (the post-call exit sites' protocol): a
    ///   thread fetching the call sees the old or the new displacement whole,
    ///   and both reach `target` (the old one through the forward). A site
    ///   straddling a word (three alignments in eight) is left forwarding.
    /// * **Serialised** with every other code patch of the cache (`mutation`
    ///   held across the protection change, as `make_not_entrant` and the
    ///   exit-poll pass hold it), `target` re-checked neither retired, nor
    ///   superseded, nor not entrant under it.
    /// * The caller is neither the patched body, nor `target`, nor retired,
    ///   nor not entrant; `target` is in `rel32` reach of the site.
    ///
    /// The caller is marked first ([`CompiledMethod::has_rebound_call_sites`]):
    /// a frame of the patched body entered from this site before the patch
    /// still returns there, and the walker must no longer name it by the
    /// displacement.
    ///
    /// Semantics: the rebound call is exactly the call the forward makes (the
    /// same `target`, the caller's arguments and return address untouched),
    /// without the forwarding prefix's flag tests; a `target` later displaced
    /// is made not entrant by its own publication, as a body any caller baked.
    ///
    /// `Ok(cleared)`: the site now calls `target`; `cleared` says the record's
    /// forward word was cleared
    /// ([`NotEntrantRecord::clear_forward_word_for_rebinds`], for the first
    /// [`STALE_CALL_REBIND_CLEARS_PER_RECORD`] sites), so the next stale
    /// caller's call comes through the VM once and is rebound too.
    pub fn rebind_stale_call_site(
        &self,
        record: &NotEntrantRecord,
        target: &Arc<CompiledMethod>,
        caller: &CompiledMethod,
        ret_pc: usize,
    ) -> Result<bool, &'static str> {
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (record, target, caller, ret_pc);
            Err("not x86-64")
        }
        #[cfg(target_arch = "x86_64")]
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            let (old_entry, old_artifact) = record.patched_body();
            // Cast: an address, compared and used for offset arithmetic.
            let new_entry = target.entry_ptr() as usize;
            if old_entry == 0 {
                return Err("the stub names no patched body");
            }
            if new_entry == 0 || new_entry == old_entry || target.artifact_id == old_artifact {
                return Err("the current body is the patched body");
            }
            if caller.artifact_id == old_artifact {
                return Err("the caller is the patched body itself");
            }
            // A self-recursive call: the current body baked its predecessor
            // (the body published when it was compiled) and displaced it at
            // its own publication. Re-pointing the call at the caller itself
            // is the call the forward makes; refusing it sent the caller to a
            // stale-caller recompile whose body baked the caller in turn, a
            // recompile per publication without end (`R21WalkthrowWalkSplit`'s
            // `deepA`: 69 compiles in one run).
            if caller.artifact_id == target.artifact_id && !stale_call_self_rebind_enabled() {
                return Err("the caller is the current body itself");
            }
            if caller.retired.load(Ordering::Acquire) {
                return Err("the caller is retired");
            }
            if caller.is_not_entrant() {
                return Err("the caller is not entrant");
            }
            let roots_old = caller._direct_callee_roots.iter().any(|root| {
                // Cast: an address, compared only.
                root.artifact_id == old_artifact && root.entry_ptr() as usize == old_entry
            });
            if !roots_old {
                return Err("the caller does not root the patched body");
            }
            // Cast: an address, for the range checks.
            let base = caller._buffer.as_ptr() as usize;
            let len = caller._buffer.pos();
            let mapped = caller._buffer.mapped_len();
            let Some(site) = ret_pc.checked_sub(5) else {
                return Err("the return address is outside the caller's code");
            };
            if site < base || ret_pc > base.saturating_add(len) {
                return Err("the return address is outside the caller's code");
            }
            // JIT r23 wave 10, lane L6 (`CRATONVM_JIT_STALE_CALL_REBIND_ABS64`):
            // the optimizing tier's `MOV RAX, imm64 ; CALL RAX` form.
            {
                // SAFETY: `[base, base + len)` is the caller's emitted code,
                // mapped and readable while `caller` is alive (the VM's pin).
                let code = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
                if let Some(imm_off) = abs64_call_imm_offset(code, ret_pc - base) {
                    if !stale_call_rebind_abs64_enabled() {
                        return Err("the site is MOV RAX, imm64 ; CALL RAX and the abs64 rebind is off");
                    }
                    return self.rebind_stale_abs64_call_site(
                        record,
                        target,
                        caller,
                        base + imm_off,
                        old_entry,
                        old_artifact,
                        new_entry,
                        base,
                        mapped,
                    );
                }
            }
            let disp_at = site + 1;
            let word = disp_at & !7usize;
            let at = disp_at - word;
            if at + 4 > 8 || word < base || word.saturating_add(8) > base.saturating_add(mapped) {
                return Err("the call's displacement straddles an 8-byte word");
            }
            // Cast: user-space addresses (below 2^47), as signed 64-bit for
            // the displacement.
            let Ok(new_rel) = i32::try_from(new_entry as i64 - ret_pc as i64) else {
                return Err("the current body is out of rel32 reach of the site");
            };
            let _mutation = self.mutation.lock();
            if target.retired.load(Ordering::SeqCst)
                || target.superseded.load(Ordering::SeqCst)
                || target.is_not_entrant()
            {
                return Err("the current body was withdrawn");
            }
            if let Some(why) = record.rebind_refusal(target) {
                return Err(why);
            }
            // SAFETY: `[site, ret_pc)` lies inside the caller's emitted code
            // (checked above), mapped and readable while `caller` is alive,
            // which the VM's pin guarantees for this call.
            let (opcode, tail) = unsafe {
                (
                    (site as *const u8).read(),
                    [
                        ((ret_pc - 2) as *const u8).read(),
                        ((ret_pc - 1) as *const u8).read(),
                    ],
                )
            };
            // `CALL r64` (`FF /2`, ModRM `D0..D7`, REX.B for R8..R15): the
            // inline cache's indirect call, whose ModRM would be read as the
            // top byte of a displacement.
            if opcode != 0xE8 || (tail[0] == 0xFF && tail[1] & 0xF8 == 0xD0) {
                return Err("the return site is not a direct CALL rel32");
            }
            // SAFETY: `[word, word + 8)` is an 8-aligned word inside the
            // caller's mapping (checked above), alive as above.
            let cell = unsafe { &*(word as *const AtomicU64) };
            let current = cell.load(Ordering::SeqCst);
            let mut bytes = current.to_le_bytes();
            let old_rel =
                i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
            // Cast: a sign-extended displacement, added with wrap-around as
            // the CPU adds it.
            if ret_pc.wrapping_add(old_rel as isize as usize) != old_entry {
                return Err("the return site's CALL does not name the patched body");
            }
            // Before the site can name `target`: see the doc comment. Listed
            // first too (lane rev16): an invalidation of `target` withdraws
            // this caller as it withdraws a caller that baked `target`; the
            // stub's forward this call replaces tested `target`'s `retired`
            // flag on every call. Under `mutation`, as the closures read it.
            {
                let mut listed = caller.rebound_callees.lock();
                if !listed.contains(&(new_entry, target.artifact_id)) {
                    listed.push((new_entry, target.artifact_id));
                }
            }
            caller.rebound_call_sites.fetch_add(1, Ordering::SeqCst);
            if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
                return Err("the caller's mapping could not be made writable");
            }
            bytes[at..at + 4].copy_from_slice(&new_rel.to_le_bytes());
            let swapped = cell
                .compare_exchange(
                    current,
                    u64::from_le_bytes(bytes),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok();
            restore_read_execute(base, mapped);
            if !swapped {
                return Err("the site's word changed under the patch");
            }
            let n = record.note_call_site_rebound();
            Ok(n <= STALE_CALL_REBIND_CLEARS_PER_RECORD && record.clear_forward_word_for_rebinds())
        }
    }

    /// JIT r23 wave 10, lane L6 (`CRATONVM_JIT_STALE_CALL_REBIND_ABS64`): the
    /// [`Self::rebind_stale_call_site`] of a `MOV RAX, imm64 ; CALL RAX` site
    /// (`48 B8 <imm64> FF D0`, the optimizing tier's direct cross call), whose
    /// imm64 starts at `imm_at`. The caller has already checked the site's
    /// place in the caller's code. Every other rule is the rel32 path's:
    /// roots, lifetime, `mutation`, the target re-checked under it, and the
    /// caller marked first.
    ///
    /// **Why the patch is safe for a thread executing the site.** The imm64
    /// must start on an 8-byte boundary (`CRATONVM_JIT_IR_DIRECT_CALL_IMM_ALIGN`
    /// pads the emitter so it does; any other site is refused). The patched
    /// word is then exactly the eight immediate bytes: no opcode byte and no
    /// other instruction shares it, so instruction boundaries never change.
    /// It is rewritten by ONE `LOCK CMPXCHG` of that naturally aligned
    /// quadword, which cannot cross a cache line. That is the same guarantee
    /// the rel32 path and HotSpot's aligned call-displacement patching
    /// (`NativeCall::set_destination_mt_safe`) rest on: another core's
    /// instruction fetch sees the old eight bytes or the new eight bytes,
    /// never a mix (Intel SDM vol. 3A 8.1.1 / 8.1.3). Without a serializing
    /// instruction an executor may still run the OLD immediate for a while,
    /// and that is correct: the old entry is the not-entrant body whose stub
    /// forwards to `target`. The record owns `target` and the caller roots the
    /// old body, so both addresses stay mapped for as long as the caller lives.
    /// A thread already past the `MOV` has RAX loaded and calls whichever
    /// entry it read; both reach `target`.
    #[cfg(target_arch = "x86_64")]
    #[allow(clippy::too_many_arguments)]
    fn rebind_stale_abs64_call_site(
        &self,
        record: &NotEntrantRecord,
        target: &Arc<CompiledMethod>,
        caller: &CompiledMethod,
        imm_at: usize,
        old_entry: usize,
        _old_artifact: u64,
        new_entry: usize,
        base: usize,
        mapped: usize,
    ) -> Result<bool, &'static str> {
        use std::sync::atomic::{AtomicU64, Ordering};
        if imm_at % 8 != 0 {
            return Err("the call's imm64 is not 8-byte aligned");
        }
        if imm_at < base || imm_at.saturating_add(8) > base.saturating_add(mapped) {
            return Err("the call's imm64 is outside the caller's mapping");
        }
        let _mutation = self.mutation.lock();
        if target.retired.load(Ordering::SeqCst)
            || target.superseded.load(Ordering::SeqCst)
            || target.is_not_entrant()
        {
            return Err("the current body was withdrawn");
        }
        if let Some(why) = record.rebind_refusal(target) {
            return Err(why);
        }
        // SAFETY: `[imm_at, imm_at + 8)` is an 8-aligned word inside the
        // caller's mapping (checked above), alive while `caller` is.
        let cell = unsafe { &*(imm_at as *const AtomicU64) };
        let current = cell.load(Ordering::SeqCst);
        // Cast: an immediate holding an address, compared as one.
        if current as usize != old_entry {
            return Err("the return site's CALL does not name the patched body");
        }
        {
            let mut listed = caller.rebound_callees.lock();
            if !listed.contains(&(new_entry, target.artifact_id)) {
                listed.push((new_entry, target.artifact_id));
            }
        }
        caller.rebound_call_sites.fetch_add(1, Ordering::SeqCst);
        if crate::platform::make_code_patchable(base as *mut u8, mapped).is_err() {
            return Err("the caller's mapping could not be made writable");
        }
        // Cast: an address, stored as the immediate the CPU loads.
        let swapped = cell
            .compare_exchange(current, new_entry as u64, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        restore_read_execute(base, mapped);
        if !swapped {
            return Err("the site's word changed under the patch");
        }
        let n = record.note_call_site_rebound();
        Ok(n <= STALE_CALL_REBIND_CLEARS_PER_RECORD && record.clear_forward_word_for_rebinds())
    }
}

/// `CRATONVM_JIT_STALE_CALL_REBIND_ABS64` (JIT r23 wave 10, lane L6; perf,
/// **default ON**, `=0` kill): [`JitCache::rebind_stale_call_site`] also
/// rebinds the optimizing tier's `MOV RAX, imm64 ; CALL RAX` direct call (an
/// 8-aligned imm64, see `ir_lower::ir_direct_call_imm_align_enabled`) instead
/// of declining it. A declined site sends its caller to a full recompile
/// (`stale-caller recompile`). Read on a stub's slow path only.
pub fn stale_call_rebind_abs64_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_STALE_CALL_REBIND_ABS64")
}

/// `CRATONVM_JIT_STALE_CALL_SELF_REBIND` (2026-10-26, the stack-walker
/// deep-walk page's recompile loop; **default ON**, `=0` restores the
/// refusal): [`JitCache::rebind_stale_call_site`] also re-points a
/// self-recursive call -- the caller is the method's current body, the stale
/// target its predecessor -- at the caller itself. Read on a not-entrant
/// stub's slow path only (cold).
pub fn stale_call_self_rebind_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_STALE_CALL_SELF_REBIND")
}

/// `CRATONVM_JIT_SP_PATCHABLE_CALL_ALIGN` (JIT r23 wave 10, lane L6; perf,
/// **default ON**, `=0` kill): the single-pass backend NOP-pads a `CALL rel32`
/// of compiled code (`x64::emit_call_absolute`, target inside a registered JIT
/// code range) so the displacement is 4-aligned. A 4-aligned rel32 never
/// straddles an 8-byte word, so [`JitCache::rebind_stale_call_site`] can
/// patch it. Before this, three alignments in eight were declined (`the
/// call's displacement straddles an 8-byte word`) and sent their caller to a
/// full recompile. Helper calls are not padded. Read per compile.
pub(crate) fn sp_patchable_call_align_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_PATCHABLE_CALL_ALIGN")
}

/// NOP bytes to emit at absolute address `at` so that a following `E8 rel32`
/// has its displacement (at `at + pad + 1`) on a 4-byte boundary: `0..=3`.
pub(crate) fn sp_patchable_call_pad(at: usize) -> usize {
    (4 - (at.wrapping_add(1) & 3)) & 3
}

#[cfg(test)]
mod r23w10_l6_sp_patchable_call_tests {
    use super::*;

    #[test]
    fn the_padded_rel32_lies_in_one_eight_byte_word() {
        for at in 0x2000usize..0x2010 {
            let pad = sp_patchable_call_pad(at);
            assert!(pad < 4);
            let disp = at + pad + 1;
            assert_eq!(disp % 4, 0, "at {at:#x}");
            assert!((disp & 7) + 4 <= 8, "the rebind's one-word rule holds at {at:#x}");
        }
    }

    #[test]
    fn the_switch_is_default_on_with_kill_word_zero() {
        const KEY: &str = "CRATONVM_JIT_SP_PATCHABLE_CALL_ALIGN";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(sp_patchable_call_align_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!sp_patchable_call_align_enabled())
        });
    }
}

/// The offset in `code` of the imm64 of a `MOV RAX, imm64 ; CALL RAX`
/// (`48 B8 <imm64> FF D0`) whose return address is `ret_off`, or `None` when
/// the twelve bytes before `ret_off` are not that sequence.
pub(crate) fn abs64_call_imm_offset(code: &[u8], ret_off: usize) -> Option<usize> {
    let start = ret_off.checked_sub(12)?;
    let site = code.get(start..ret_off)?;
    (site[0] == 0x48 && site[1] == 0xB8 && site[10] == 0xFF && site[11] == 0xD0)
        .then_some(start + 2)
}

#[cfg(test)]
mod r23w10_l6_abs64_rebind_tests {
    use super::*;

    #[test]
    fn the_abs64_call_decodes_its_immediate() {
        let mut code = vec![0x90u8, 0x90];
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0]);
        let ret = code.len();
        assert_eq!(abs64_call_imm_offset(&code, ret), Some(4));
    }

    #[test]
    fn other_sequences_are_not_abs64_calls() {
        // CALL R11 (`41 FF D3`) behind a MOV R11, imm64.
        let mut code = vec![0x49u8, 0xBB];
        code.extend_from_slice(&[0; 8]);
        code.extend_from_slice(&[0x41, 0xFF, 0xD3]);
        assert_eq!(abs64_call_imm_offset(&code, code.len()), None);
        // E8 rel32, and too short a prefix.
        let e8 = [0xE8u8, 1, 2, 3, 4];
        assert_eq!(abs64_call_imm_offset(&e8, 5), None);
        assert_eq!(abs64_call_imm_offset(&[0xFF, 0xD0], 2), None);
        // Past the end of the code.
        assert_eq!(abs64_call_imm_offset(&e8, 20), None);
    }

    #[test]
    fn the_switch_is_default_on_with_kill_word_zero() {
        const KEY: &str = "CRATONVM_JIT_STALE_CALL_REBIND_ABS64";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(stale_call_rebind_abs64_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!stale_call_rebind_abs64_enabled())
        });
    }
}

/// STASH20-2: how many rebound call sites of one not-entrant record clear its
/// forward word ([`JitCache::rebind_stale_call_site`]). Each clear sends the
/// next stale caller's first call through the VM's by-name re-dispatch (one
/// microsecond-scale call per call site, as HotSpot's wrong-method stub
/// resolves each site once); past it the forward stays, and later stale
/// callers keep forwarding as before.
pub const STALE_CALL_REBIND_CLEARS_PER_RECORD: u32 = 64;

/// Interpreter round i1 wave 22, lane L6: the stub's ABI, the patch, and the
/// cache's redefinition pass, on hand-built bodies (the single-pass body is
/// covered in `x64/tests.rs`).
#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::ExecutableBuffer;

    /// TF15-2 (round 16 wave 16, lane chains2): the re-dispatch line is due at
    /// the first call and at every power of two, with the running count.
    #[test]
    fn redispatch_lines_are_due_at_powers_of_two() {
        let r = record(2, false);
        assert_eq!(r.redispatch_count(), 0);
        let due: Vec<u64> = (0..20).filter_map(|_| r.note_redispatch()).collect();
        assert_eq!(due, vec![1, 2, 4, 8, 16]);
        assert_eq!(r.redispatch_count(), 20);
    }

    /// HANDLER11-5 (JIT round 19 wave 12, lane handler12): a body a
    /// caller-held site bound forwards only to a body bound the same way, in
    /// both the supersede's static half and the stub's dynamic half.
    #[test]
    fn a_caller_held_sites_body_forwards_only_to_a_bound_body() {
        let old = answering(1);
        let new = answering(2);
        for body in [&old, &new] {
            body.stamp_published_key(&Arc::from("h12/S"), &Arc::from("m"), &Arc::from("()I"), 7);
        }
        let mut old = old;
        let mut new = new;
        old.entry_java_args = 0;
        new.entry_java_args = 0;
        assert!(
            caller_held_forward_refusal(false, &new).is_none(),
            "an unbound body forwards"
        );
        old.mark_bound_by_a_caller_held_site();
        assert!(caller_held_forward_refusal(true, &new).is_some());
        assert!(
            !supersede_forward_admissible(&old, &new),
            "the supersede leaves a caller-held site's body entrant"
        );
        new.mark_bound_by_a_caller_held_site();
        assert!(caller_held_forward_refusal(true, &new).is_none());
        assert!(supersede_forward_admissible(&old, &new));
        let mut rec = record(0, false);
        rec.caller_held_bound = true;
        let unbound = answering(3);
        unbound.stamp_published_key(&Arc::from("h12/S"), &Arc::from("m"), &Arc::from("()I"), 7);
        let mut unbound = unbound;
        unbound.entry_java_args = 0;
        assert_eq!(
            rec.forward_target_refusal(&unbound),
            Some("the patched body is a caller-held site's; target not bound by one")
        );
    }

    /// `NOP5; MOV EAX, 7; RET`: a body that answers 7 until it is patched.
    fn seven() -> CompiledMethod {
        answering(7)
    }

    /// `NOP5; MOV EAX, value; RET`.
    fn answering(value: u8) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]);
        CompiledMethod::new(buf)
    }

    fn record(num_java_args: usize, needs_context: bool) -> Box<NotEntrantRecord> {
        Box::new(NotEntrantRecord::new(
            &Arc::from("i22l6/C"),
            &Arc::from("m"),
            &Arc::from("(JJJJJJJJ)J"),
            0x2206,
            true,
            0,
            needs_context,
            num_java_args,
        ))
    }

    /// Stands in for the VM's `jit_not_entrant_entry`: folds what the stub
    /// passed into one number, so the call's result proves every argument
    /// arrived in order, register-passed and stack-passed alike.
    extern "C" fn fold_args(record: i64, regs: i64, stack_args: i64) -> i64 {
        // SAFETY: the stub passes the live record it baked and the spill and
        // stack pointers of the call it intercepted.
        let rec = unsafe { &*(record as *const NotEntrantRecord) };
        // SAFETY: as above.
        let args = unsafe { rec.java_args(regs as *const i64, stack_args as *const i64) };
        // SAFETY: as above.
        let ctx = unsafe { rec.context_arg(regs as *const i64) }.unwrap_or(0);
        let folded: i64 = args
            .iter()
            .enumerate()
            .map(|(i, v)| v * (i as i64 + 1))
            .sum();
        folded + 1000 * rec.num_java_args as i64 + 100_000 * ctx
    }

    #[test]
    fn a_patched_entry_hands_every_argument_to_the_helper_and_returns_its_answer() {
        let body = seven();
        // SAFETY: the body ignores its arguments and returns 7.
        let before = unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call");
        assert_eq!(before, 7);
        assert!(!body.is_not_entrant());
        body.install_not_entrant_stub(record(8, false), fold_args as usize)
            .expect("patch");
        assert!(body.is_not_entrant());
        assert_eq!(body.code_bytes()[0], 0xE9, "the entry is a JMP now");
        // SAFETY: the entry now jumps to the stub, which calls `fold_args`.
        let after = unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call");
        // 1*1 + 2*2 + ... + 8*8 = 204, plus the count.
        assert_eq!(after, 204 + 8000);
        // Again: the patch is permanent, and a second patch is refused
        // without touching it.
        // SAFETY: as above.
        assert_eq!(
            unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call"),
            8204
        );
        assert_eq!(
            body.install_not_entrant_stub(record(8, false), fold_args as usize),
            Err(NotEntrantRefusal::NoEntryPad)
        );
    }

    #[test]
    fn a_context_body_passes_the_context_before_its_java_arguments() {
        let body = seven();
        body.install_not_entrant_stub(record(2, true), fold_args as usize)
            .expect("patch");
        // SAFETY: the entry jumps to the stub, which calls `fold_args`.
        let got = unsafe { body.try_call(&[3, 10, 20]) }.expect("call");
        assert_eq!(got, 10 + 2 * 20 + 2000 + 300_000);
    }

    #[test]
    fn a_body_without_the_pad_or_an_identity_is_left_alone() {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
        let bare = CompiledMethod::new(buf);
        assert_eq!(
            bare.install_not_entrant_stub(record(0, false), fold_args as usize),
            Err(NotEntrantRefusal::NoEntryPad)
        );
        // SAFETY: unpatched: MOV EAX, 7; RET.
        assert_eq!(unsafe { bare.try_call(&[]) }.expect("call"), 7);
        let hook = NotEntrantHook {
            helper: fold_args as usize,
            vm_ptr: 0,
        };
        assert_eq!(
            seven().make_not_entrant(hook),
            Err(NotEntrantRefusal::NoIdentity),
            "never published: nothing names the method"
        );
    }

    /// The redefinition pass: a stale body is collected while published,
    /// refused while it still is, and patched once the eviction withdrew it.
    #[test]
    fn a_withdrawn_stale_body_is_made_not_entrant_and_a_published_one_is_not() {
        extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
            // SAFETY: the stub passes the live record it baked.
            let rec = unsafe { &*(record as *const NotEntrantRecord) };
            1000 + i64::from(rec.info.invoke_kind)
        }
        let cache = JitCache::new();
        let class = cratonvm_types::ClassId::new(0x2206);
        let mut body = seven();
        body.entry_java_args = 0;
        cache.put(
            Arc::from("i22l6/C"),
            Arc::from("m"),
            Arc::from("()I"),
            class,
            body,
        );
        let stale = cache.redefinition_stale_bodies(class, "i22l6/C", false);
        assert_eq!(stale.len(), 1);
        assert_eq!(cache.make_not_entrant(&stale), 0, "not armed: nothing");
        cache.arm_not_entrant(NotEntrantHook {
            helper: kind as usize,
            vm_ptr: 0,
        });
        assert_eq!(cache.make_not_entrant(&stale), 0, "still published");
        assert!(!stale[0].is_not_entrant());
        let (withdrawn, _) = cache.invalidate_for_redefinition(class, "i22l6/C");
        assert_eq!(withdrawn, 1);
        assert_eq!(cache.make_not_entrant(&stale), 1);
        assert!(stale[0].is_not_entrant());
        // SAFETY: the entry jumps to the stub, which calls `kind`.
        let got = unsafe { stale[0].try_call(&[]) }.expect("call");
        assert_eq!(
            got, 1003,
            "a static method is re-dispatched as invokestatic"
        );
        assert_eq!(cache.not_entrant_counts().0, 1);
        // Wave 23: withdrawn by the redefinition, for the back-edge poll.
        assert!(stale[0].is_withdrawn_by_redefinition());
        assert_eq!(cache.bodies_withdrawn_by_redefinition(), 1);
        for b in stale {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Round 16 wave 15 (lane trapfix): a body a deoptimization's eviction
    /// withdrew (`JitCache::remove`) is made not entrant without the
    /// redefinition's bookkeeping -- not marked withdrawn by a redefinition,
    /// the redefinition count unmoved -- and a published one is still refused.
    #[test]
    fn an_evicted_body_is_made_not_entrant_without_the_redefinition_marks() {
        extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
            // SAFETY: the stub passes the live record it baked.
            let rec = unsafe { &*(record as *const NotEntrantRecord) };
            2000 + i64::from(rec.info.invoke_kind)
        }
        let cache = JitCache::new();
        cache.arm_not_entrant(NotEntrantHook {
            helper: kind as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x1615);
        let mut body = seven();
        body.entry_java_args = 0;
        cache.put(
            Arc::from("r16w15/C"),
            Arc::from("m"),
            Arc::from("()I"),
            class,
            body,
        );
        let live = cache.get("r16w15/C", "m", "()I", class).expect("published");
        assert_eq!(
            cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
            0,
            "still published"
        );
        assert!(!live.is_not_entrant());
        cache.remove("r16w15/C", "m", "()I", class);
        assert!(cache.get("r16w15/C", "m", "()I", class).is_none());
        assert_eq!(
            cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
            1
        );
        assert!(live.is_not_entrant());
        // SAFETY: the entry jumps to the stub, which calls `kind`.
        let got = unsafe { live.try_call(&[]) }.expect("call");
        assert_eq!(
            got, 2003,
            "a static method is re-dispatched as invokestatic"
        );
        assert!(!live.is_withdrawn_by_redefinition());
        assert_eq!(cache.bodies_withdrawn_by_redefinition(), 0);
        // Idempotent: a second eviction pass patches nothing more.
        assert_eq!(
            cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
            0
        );
        assert_eq!(cache.not_entrant_counts().0, 1);
        crate::defer_jit_owner(Some(live));
    }

    /// t1 wave 5, lane X2 (`CRATONVM_JIT_INVALIDATE_NOT_ENTRANT`, opt-in): a
    /// dependency invalidation makes the method-entry body it withdrew not
    /// entrant (eviction flavour), so a caller that baked its entry -- an
    /// optimizing caller has no retire cell -- re-dispatches. Off, or with
    /// the hook unarmed, the body is withdrawn and left as it was.
    #[test]
    fn t1x2_a_dependency_invalidation_makes_the_withdrawn_body_not_entrant() {
        extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
            // SAFETY: the stub passes the live record it baked.
            let rec = unsafe { &*(record as *const NotEntrantRecord) };
            3000 + i64::from(rec.info.invoke_kind)
        }
        let publish = |cache: &JitCache, class: cratonvm_types::ClassId, name: &str| {
            let mut body = seven();
            body.entry_java_args = 0;
            body.inlined_methods = vec![("t1x2/Shape".into(), "area".into(), "()I".into())];
            cache.put(Arc::from(name), Arc::from("m"), Arc::from("()I"), class, body);
            cache.get(name, "m", "()I", class).expect("published")
        };
        let run = |on: bool, armed: bool, id: u32, name: &str| -> (bool, bool) {
            let _flags = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_JIT_INVALIDATE_NOT_ENTRANT",
                    on.then_some("1"),
                )]),
            );
            let cache = JitCache::new();
            if armed {
                cache.arm_not_entrant(NotEntrantHook {
                    helper: kind as usize,
                    vm_ptr: 0,
                });
            }
            let class = cratonvm_types::ClassId::new(id);
            let live = publish(&cache, class, name);
            assert_eq!(cache.invalidate_for_class_change("t1x2/Shape"), 1);
            assert!(cache.get(name, "m", "()I", class).is_none(), "withdrawn");
            let patched = live.is_not_entrant();
            let answer_is_stub = patched && {
                // SAFETY: the entry jumps to the stub, which calls `kind`.
                unsafe { live.try_call(&[]) }.ok() == Some(3003)
            };
            assert!(!live.is_withdrawn_by_redefinition());
            crate::defer_jit_owner(Some(live));
            (patched, answer_is_stub)
        };
        assert_eq!(run(true, true, 0x7105, "t1x2/A"), (true, true));
        assert_eq!(run(false, true, 0x7106, "t1x2/B"), (false, false));
        assert_eq!(run(true, false, 0x7107, "t1x2/C"), (false, false));
    }

    /// The switch is opt-in.
    #[test]
    fn t1x2_the_invalidate_not_entrant_switch_is_opt_in() {
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_INVALIDATE_NOT_ENTRANT",
                None,
            )]),
        );
        assert!(!crate::jit_invalidate_not_entrant_enabled());
    }

    /// j27 wave 5 (lane X2LCAP; `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT_SUPERSEDED`,
    /// default ON, `=0` kill, under the opt-in parent above): a dependency
    /// invalidation that retires a SUPERSEDED method-entry body -- a C1 body a
    /// tier-up displaced whose forward was never patched, still baked into an
    /// optimizing caller -- makes it not entrant too, so that caller
    /// re-dispatches. Before, only the bodies the maps held were patched, and
    /// the retired superseded body stayed enterable.
    #[test]
    fn j27_x2lcap_a_retired_superseded_body_is_made_not_entrant() {
        extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
            // SAFETY: the stub passes the live record it baked.
            let rec = unsafe { &*(record as *const NotEntrantRecord) };
            4000 + i64::from(rec.info.invoke_kind)
        }
        let body = |inlines: bool| {
            let mut cm = seven();
            cm.entry_java_args = 0;
            if inlines {
                cm.inlined_methods = vec![("j27x2lcap/Shape".into(), "area".into(), "()I".into())];
            }
            cm
        };
        let run = |superseded_switch: Option<&str>, id: u32, name: &str| -> (bool, bool) {
            let _flags = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[
                    ("CRATONVM_JIT_INVALIDATE_NOT_ENTRANT", Some("1")),
                    ("CRATONVM_JIT_INVALIDATE_NOT_ENTRANT_SUPERSEDED", superseded_switch),
                    // The armed cache's supersede forward (default ON) would patch
                    // `old` at the second `put`; this test pins the invalidation's patch.
                    ("CRATONVM_JIT_SUPERSEDE_REDIRECT", Some("0")),
                ]),
            );
            let mut cache = JitCache::new();
            cache.invalidation_retires_superseded = true;
            cache.arm_not_entrant(NotEntrantHook {
                helper: kind as usize,
                vm_ptr: 0,
            });
            let class = cratonvm_types::ClassId::new(id);
            cache.put(Arc::from(name), Arc::from("m"), Arc::from("()I"), class, body(true));
            let old = cache.get(name, "m", "()I", class).expect("published");
            cache.put(Arc::from(name), Arc::from("m"), Arc::from("()I"), class, body(false));
            assert!(old.superseded.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!old.is_not_entrant(), "no supersede forward in a bare cache");
            cache.invalidate_for_class_change("j27x2lcap/Shape");
            let retired = old.retired.load(std::sync::atomic::Ordering::SeqCst);
            let redispatches = old.is_not_entrant() && {
                // SAFETY: a patched entry jumps to the stub, which calls `kind`.
                unsafe { old.try_call(&[]) }.ok() == Some(4003)
            };
            assert!(!old.is_withdrawn_by_redefinition(), "eviction flavour");
            crate::defer_jit_owner(Some(old));
            (retired, redispatches)
        };
        assert_eq!(run(None, 0x7271, "j27x2lcap/A"), (true, true), "default ON");
        assert_eq!(run(Some("0"), 0x7272, "j27x2lcap/B"), (true, false), "=0 kill");
    }

    /// The superseded half is default ON (it only acts under the opt-in parent).
    #[test]
    fn j27_x2lcap_the_superseded_half_is_default_on() {
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_INVALIDATE_NOT_ENTRANT_SUPERSEDED",
                None,
            )]),
        );
        assert!(crate::jit_invalidate_not_entrant_superseded_enabled());
    }

    /// Round 16 wave 15 (lane trapfix): a refused eviction patch does not hold
    /// the redefinition's unpatched constant-pool floor.
    #[test]
    fn a_refused_eviction_patch_holds_no_unpatched_stamp_floor() {
        let cache = JitCache::new();
        cache.arm_not_entrant(NotEntrantHook {
            helper: fold_args as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x1616);
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
        let mut bare = CompiledMethod::new(buf);
        bare.entry_java_args = 0;
        bare.compile_cp_stamp = Some(17);
        cache.put(
            Arc::from("r16w15/D"),
            Arc::from("m"),
            Arc::from("()I"),
            class,
            bare,
        );
        let live = cache.get("r16w15/D", "m", "()I", class).expect("published");
        cache.remove("r16w15/D", "m", "()I", class);
        assert_eq!(
            cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
            0,
            "no entry pad: refused"
        );
        assert_eq!(cache.min_unpatched_cp_stamp(), None);
        assert!(!live.is_withdrawn_by_redefinition());
        crate::defer_jit_owner(Some(live));
    }

    /// Wave 25 (lane L6): a body the pass retires but cannot patch holds the
    /// cache's unpatched floor at its compile's constant-pool stamp, once,
    /// for as long as it lives
    /// (`interpreter-L6-a-refused-not-entrant-patch-leaves-an-old-body-below-the-census-floor-FIXED`).
    #[test]
    fn a_refused_body_holds_the_unpatched_stamp_floor_while_it_lives() {
        let cache = JitCache::new();
        cache.arm_not_entrant(NotEntrantHook {
            helper: fold_args as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x2506);
        // No entry pad: the patch is refused (`NoEntryPad`).
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
        let mut bare = CompiledMethod::new(buf);
        bare.entry_java_args = 0;
        bare.compile_cp_stamp = Some(41);
        cache.put(
            Arc::from("i25l6/C"),
            Arc::from("m"),
            Arc::from("()I"),
            class,
            bare,
        );
        let stale = cache.redefinition_stale_bodies(class, "i25l6/C", false);
        assert_eq!(stale.len(), 1);
        let _ = cache.invalidate_for_redefinition(class, "i25l6/C");
        assert_eq!(cache.min_unpatched_cp_stamp(), None);
        assert_eq!(cache.make_not_entrant(&stale), 0, "refused");
        assert!(stale[0].is_withdrawn_by_redefinition());
        assert_eq!(cache.min_unpatched_cp_stamp(), Some(41));
        assert_eq!(cache.make_not_entrant(&stale), 0);
        assert_eq!(cache.unpatched_bodies.lock().len(), 1, "once per body");
        // A lower stamp wins while its body lives, and is forgotten after.
        let mut older = seven();
        older.compile_cp_stamp = Some(3);
        let older = Arc::new(older);
        cache.note_unpatched(&older);
        assert_eq!(cache.min_unpatched_cp_stamp(), Some(3));
        drop(older);
        assert_eq!(cache.min_unpatched_cp_stamp(), Some(41));
        for b in stale {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Wave 25 (lane L6), stage 1 of
    /// `i20-L1-proposal-per-body-copied-bytecode-dependencies`: a copy the VM
    /// notes while a compilation is open on the thread is carried by every
    /// body built inside it (a nested compile shares the list), by none built
    /// before or after, and a scoped redefinition of the copied class
    /// collects the body that carries it.
    #[test]
    fn a_copy_noted_in_an_open_compile_is_carried_by_its_bodies() {
        let cache = JitCache::new();
        let copied = cratonvm_types::ClassId::new(0x2507);
        let caller = cratonvm_types::ClassId::new(0x2508);
        let before = seven();
        let (inside, nested) = {
            let _outer = crate::open_compile_epoch_witness();
            cache.note_bytecode_copied(copied);
            let nested = {
                let _inner = crate::open_compile_epoch_witness();
                seven()
            };
            (seven(), nested)
        };
        let after = seven();
        assert!(inside.copied_bytecode_of(copied));
        assert!(
            nested.copied_bytecode_of(copied),
            "a nested compile shares the list"
        );
        assert!(!inside.copied_bytecode_of(caller));
        assert!(!before.copied_bytecode_of(copied));
        assert!(
            !after.copied_bytecode_of(copied),
            "the outermost witness closed the list"
        );
        assert_eq!(
            cache.bytecode_was_copied(copied),
            !crate::SCOPE_REDEFINITIONS_BY_COPIED_CLASSES,
            "the per-VM mark, unless stage 3 is on"
        );
        // Noted with no compilation open: always the per-VM mark.
        let unscoped = cratonvm_types::ClassId::new(0x2509);
        cache.note_bytecode_copied(unscoped);
        assert!(cache.bytecode_was_copied(unscoped));

        let mut inside = inside;
        inside.entry_java_args = 0;
        cache.put(
            Arc::from("i25l6/Caller"),
            Arc::from("run"),
            Arc::from("()I"),
            caller,
            inside,
        );
        let stale = cache.redefinition_stale_bodies(copied, "i25l6/Copied", false);
        assert_eq!(stale.len(), 1, "the body that copied the class");
        assert!(cache
            .redefinition_stale_bodies(unscoped, "i25l6/Other", false)
            .is_empty());
        for b in stale {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// A tier-up leaves the old body out of every map, but a caller that
    /// baked it keeps entering it; the scan finds it through that caller's
    /// roots and patches it with the published one.
    #[test]
    fn a_superseded_body_only_a_baked_caller_reaches_is_made_not_entrant_too() {
        extern "C" fn answer(record: i64, _regs: i64, _stack: i64) -> i64 {
            // SAFETY: the stub passes the live record it baked.
            let rec = unsafe { &*(record as *const NotEntrantRecord) };
            2000 + i64::from(rec.info.invoke_kind)
        }
        let cache = JitCache::new();
        let target = "i22l6/Superseded";
        let caller = "i22l6/SupersededCaller";
        let redefined = cratonvm_types::ClassId::new(0x2207);
        let callers = cratonvm_types::ClassId::new(0x2208);
        let put =
            |class: &str, name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
                body.entry_java_args = 0;
                cache.put(
                    Arc::from(class),
                    Arc::from(name),
                    Arc::from("()I"),
                    id,
                    body,
                );
            };
        put(target, "own", redefined, seven());
        let old = cache
            .get(target, "own", "()I", redefined)
            .expect("published");
        let mut direct = seven();
        // Cast: an address, as the compile doors record a baked callee.
        direct._direct_callee_entries = vec![old.entry_ptr() as usize];
        put(caller, "direct", callers, direct);
        assert!(
            cache.get(caller, "direct", "()I", callers).is_some(),
            "the caller publishes, rooting the old body"
        );
        // The tier-up: a new body under the same key.
        put(target, "own", redefined, seven());
        let stale = cache.redefinition_stale_bodies(redefined, target, false);
        assert_eq!(
            stale.len(),
            2,
            "the published body, and the superseded one its caller roots"
        );
        cache.arm_not_entrant(NotEntrantHook {
            helper: answer as usize,
            vm_ptr: 0,
        });
        let _ = cache.invalidate_for_redefinition(redefined, target);
        assert_eq!(cache.make_not_entrant(&stale), 2);
        assert!(old.is_not_entrant());
        assert!(old.retired.load(std::sync::atomic::Ordering::Acquire));
        // SAFETY: the old entry now jumps to its stub, which calls `answer`.
        assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 2003);
        crate::defer_jit_owner(Some(old));
        for b in stale {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Interpreter round i1 wave 23, lane L6: the stub's forwarding prefix is
    /// as long as its documentation says, and a buffer emitted to its last
    /// byte still has room for the whole stub past a 16-byte boundary.
    #[test]
    fn the_forwarding_stub_fits_the_tail_of_a_full_buffer() {
        let plain = not_entrant_stub_bytes(0x1111, 0x2222, 0, false);
        let forwarding = not_entrant_stub_bytes(0x1111, 0x2222, 0x3333, false);
        // Wave 25: the one-compare form, for a layout with the two flags side
        // by side -- 32 bytes, both early-outs on the slow path.
        let combined = not_entrant_stub_bytes(0x1111, 0x2222, 0x3333, true);
        assert_eq!(combined.len(), plain.len() + 32);
        assert_eq!(
            &combined[22..27],
            &[0x66, 0x41, 0x83, 0x3A, 0x00],
            "CMP WORD [R10], 0"
        );
        assert_eq!(
            &combined[29..32],
            &[0x41, 0xFF, 0x23],
            "JMP [R11] closes it"
        );
        for (at, op) in [(16usize, 0x74u8), (27, 0x75)] {
            assert_eq!(combined[at], op);
            assert_eq!(at + 2 + usize::from(combined[at + 1]), 32);
        }
        assert_eq!(&combined[32..], &plain[..]);
        assert_eq!(
            not_entrant_stub_bytes(0x1111, 0x2222, 0, true),
            plain,
            "no forward word, no prefix"
        );
        assert_eq!(forwarding.len(), plain.len() + 41);
        assert!(forwarding.len() <= NOT_ENTRANT_STUB_MAX);
        assert_eq!(&forwarding[..2], &[0x49, 0xBB], "MOV R11, imm64 opens it");
        assert_eq!(
            &forwarding[38..41],
            &[0x41, 0xFF, 0x23],
            "JMP [R11] closes it"
        );
        // Every early-out lands on the slow path's first byte.
        for (at, op) in [(16usize, 0x74u8), (26, 0x75), (36, 0x75)] {
            assert_eq!(forwarding[at], op);
            assert_eq!(at + 2 + usize::from(forwarding[at + 1]), 41);
        }
        assert_eq!(&forwarding[41..], &plain[..]);
        // 49 bytes: the next 16-byte boundary is 15 bytes further on.
        let mut buf = ExecutableBuffer::new(49).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
        buf.emit(&[0x90; 38]);
        assert_eq!(buf.pos(), 49);
        let full = CompiledMethod::new(buf);
        full.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("the tail holds the forwarding stub");
        // SAFETY: the entry jumps to the stub; no forward yet, so the helper.
        assert_eq!(unsafe { full.try_call(&[10, 20]) }.expect("call"), 2050);
    }

    /// The forward: once the record names the body now published under its
    /// key, the stub jumps to it (the caller gets that body's answer, not the
    /// helper's), and falls back to the helper as soon as that body is
    /// retired or superseded.
    #[test]
    fn a_patched_entry_forwards_to_the_current_body_until_it_is_withdrawn() {
        use std::sync::atomic::Ordering;
        let cache = JitCache::new();
        let class = cratonvm_types::ClassId::new(0x2206);
        let mut nine = answering(9);
        nine.entry_java_args = 2;
        cache.put(
            Arc::from("i22l6/C"),
            Arc::from("m"),
            Arc::from("(JJJJJJJJ)J"),
            class,
            nine,
        );
        let current = cache
            .get("i22l6/C", "m", "(JJJJJJJJ)J", class)
            .expect("published");
        let old = seven();
        old.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        // SAFETY: the entry jumps to the stub, whose forward word is empty.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
        let rec = old.not_entrant.get().expect("patched");
        assert!(rec.forward_to(&current));
        assert!(rec.forward_to(&current), "idempotent");
        assert!(rec
            .forward_target()
            .is_some_and(|t| Arc::ptr_eq(&t, &current)));
        // SAFETY: the stub now jumps to `current`: MOV EAX, 9; RET.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 9);
        current.retired.store(true, Ordering::SeqCst);
        // SAFETY: retired: the stub's slow path, the helper.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
        current.retired.store(false, Ordering::SeqCst);
        current.superseded.store(true, Ordering::SeqCst);
        // SAFETY: as above.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
        assert!(
            !rec.forward_to(&current),
            "a superseded body is not admitted"
        );
        current.superseded.store(false, Ordering::SeqCst);
        // SAFETY: back: the same forward serves again.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 9);
        drop(old);
        crate::defer_jit_owner(Some(current));
    }

    /// JIT round 21 wave 10 (lane splice; review w68 item 2): a record's
    /// first forward box moves the process-wide generation (the trace walk's
    /// raw-walk memo keys `ever_forwarded` through it), and the bump is made
    /// under the forwards lock before the box and the word are published.
    ///
    /// JIT round 21 wave 13 (lane trace; review w13 item 4): the dynamic half
    /// counts the bump on the record (`first_forward_bumps`, test only),
    /// because the process word moves under every concurrent publishing test
    /// and `generation > before` could not fail. A removed bump now fails it:
    /// the count must be exactly 1 after the first forward, stay 1 on a
    /// repeated forward, and be 0 under the switch's kill word; the source
    /// pin below keeps the count beside the real `fetch_add`.
    #[test]
    fn r21_the_first_forward_moves_the_jit_cache_generation() {
        use std::sync::atomic::Ordering;
        let cache = JitCache::new();
        let class = cratonvm_types::ClassId::new(0x2206);
        let mut nine = answering(9);
        nine.entry_java_args = 2;
        cache.put(
            Arc::from("i22l6/C"),
            Arc::from("m"),
            Arc::from("(JJJJJJJJ)J"),
            class,
            nine,
        );
        let current = cache
            .get("i22l6/C", "m", "(JJJJJJJJ)J", class)
            .expect("published");
        let old = seven();
        old.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        let rec = old.not_entrant.get().expect("patched");
        assert!(!rec.ever_forwarded());
        assert_eq!(rec.first_forward_bumps.load(Ordering::Relaxed), 0);
        assert!(rec.forward_to(&current));
        assert!(rec.ever_forwarded());
        assert_eq!(
            rec.first_forward_bumps.load(Ordering::Relaxed),
            1,
            "the first forward moved the generation"
        );
        assert!(rec.forward_to(&current), "the same forward is kept");
        assert_eq!(
            rec.first_forward_bumps.load(Ordering::Relaxed),
            1,
            "only the first box moves it"
        );
        // The kill word: the forward is installed, the generation is not moved.
        {
            let _flag = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST",
                    Some("0"),
                )]),
            );
            let other = seven();
            other
                .install_not_entrant_stub(record(2, false), fold_args as usize)
                .expect("patch");
            let off = other.not_entrant.get().expect("patched");
            assert!(off.forward_to(&current));
            assert_eq!(
                off.first_forward_bumps.load(Ordering::Relaxed),
                0,
                "=0 moves nothing"
            );
            drop(other);
        }
        drop(old);
        crate::defer_jit_owner(Some(current));

        let src = include_str!("not_entrant.rs");
        let body = src
            .split("fn install_forward(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("install_forward");
        let lock = body
            .find("let mut kept = self.forwards.lock();")
            .expect("the lock");
        let bump = body
            .find("crate::JIT_CACHE_GENERATION.fetch_add(1, Ordering::Release);")
            .expect("the bump");
        let push = body.find("kept.push(forward);").expect("the push");
        let word = body
            .find("self.forward.store(word, Ordering::Release);")
            .expect("the word");
        assert!(lock < bump && bump < push && push < word);
        assert!(body.contains("if first_forward_bumps && kept.is_empty() {"));
        // The record-local count sits right after the real bump, so neither
        // can be dropped without failing the dynamic or the source half.
        assert!(body.contains(
            "crate::JIT_CACHE_GENERATION.fetch_add(1, Ordering::Release);\n            \
             #[cfg(test)]\n            \
             self.first_forward_bumps.fetch_add(1, Ordering::Relaxed);"
        ));
    }

    /// hm5: the supersede patch is asked only for a C2 body the stub could
    /// forward to -- same arity, same ABI, not wrapped, no `invokedynamic`
    /// trap, a different body -- and never of an already patched C1 body.
    #[test]
    fn hm5_a_supersede_is_patched_only_where_its_stub_can_forward() {
        let with_args = |mut body: CompiledMethod, n: u16| {
            body.entry_java_args = n;
            body
        };
        let old = with_args(answering(7), 2);
        let new = with_args(answering(9), 2);
        assert!(super::supersede_forward_admissible(&old, &new));
        assert!(
            !super::supersede_forward_admissible(&old, &old),
            "the same body"
        );
        assert!(
            !super::supersede_forward_admissible(&old, &with_args(answering(9), 3)),
            "another arity"
        );
        assert!(
            !super::supersede_forward_admissible(&old, &with_args(answering(9), u16::MAX)),
            "an unstamped arity"
        );
        let mut ctx_buf = ExecutableBuffer::new(64).expect("executable buffer");
        ctx_buf.emit(&ENTRY_PATCH_PAD);
        ctx_buf.emit(&[0xB8, 0x09, 0x00, 0x00, 0x00, 0xC3]);
        let ctx = with_args(CompiledMethod::new_with_context(ctx_buf), 2);
        assert!(
            !super::supersede_forward_admissible(&old, &ctx),
            "another ABI (a context pointer)"
        );
        let mut wrapped = with_args(answering(9), 2);
        wrapped.requires_wrapped_entry = true;
        assert!(
            !super::supersede_forward_admissible(&old, &wrapped),
            "wrapped"
        );
        let mut indy = with_args(answering(9), 2);
        indy.has_indy_trap = true;
        assert!(
            !super::supersede_forward_admissible(&old, &indy),
            "indy trap"
        );
        let patched = with_args(seven(), 2);
        patched
            .install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        assert!(
            !super::supersede_forward_admissible(&patched, &new),
            "already not entrant"
        );
    }

    /// Admission: another key, another arity or another ABI never forwards,
    /// and neither does a target that reaches the patched body through its
    /// baked callee roots (a strong-reference cycle).
    #[test]
    fn a_forward_is_refused_for_another_method_shape_or_a_cycle() {
        let cache = JitCache::new();
        let class = cratonvm_types::ClassId::new(0x2206);
        let put = |name: &str, args: u16, mut body: CompiledMethod| {
            body.entry_java_args = args;
            cache.put(
                Arc::from("i22l6/C"),
                Arc::from(name),
                Arc::from("(JJJJJJJJ)J"),
                class,
                body,
            );
            cache
                .get("i22l6/C", name, "(JJJJJJJJ)J", class)
                .expect("published")
        };
        let other_name = put("other", 2, answering(9));
        let other_arity = put("m", 3, answering(9));
        let old = seven();
        old.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        let rec = old.not_entrant.get().expect("patched");
        assert!(!rec.forward_to(&other_name), "another method");
        assert!(!rec.forward_to(&other_arity), "another arity");
        let mut ctx_buf = ExecutableBuffer::new(64).expect("executable buffer");
        ctx_buf.emit(&ENTRY_PATCH_PAD);
        ctx_buf.emit(&[0xB8, 0x09, 0x00, 0x00, 0x00, 0xC3]);
        let ctx = put("m", 2, CompiledMethod::new_with_context(ctx_buf));
        assert!(!rec.forward_to(&ctx), "another ABI (a context pointer)");
        assert!(rec.forward_target().is_none());

        // The cycle: a published callee `own`, patched, and a body for the
        // record's key that baked a call to it.
        let own = put("own", 2, answering(5));
        own.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        let mut caller = answering(9);
        // Cast: an address, as the compile doors record a baked callee.
        caller._direct_callee_entries = vec![own.entry_ptr() as usize];
        let caller = put("m", 2, caller);
        assert!(
            caller
                ._direct_callee_roots
                .iter()
                .any(|r| Arc::ptr_eq(r.arc(), &own)),
            "publication roots the baked callee"
        );
        let own_rec = own.not_entrant.get().expect("patched");
        assert!(
            !own_rec.forward_to(&caller),
            "`caller` reaches the patched body: a cycle"
        );
        for b in [other_name, other_arity, ctx, own, caller] {
            crate::defer_jit_owner(Some(b));
        }
        drop(old);
    }

    /// str16: through the cache, the same cycle IS forwarded -- registered by
    /// the target's entry and released when the target is superseded, so the
    /// cycle cannot keep it mapped -- and the stub's word goes back to `0`.
    #[test]
    fn str16_a_cycle_forward_is_installed_and_released_on_supersede() {
        let cache = JitCache::new();
        // The class id `record` names.
        let class = cratonvm_types::ClassId::new(0x2206);
        let put = |name: &str, body: CompiledMethod| {
            let mut body = body;
            body.entry_java_args = 2;
            cache.put(
                Arc::from("i22l6/C"),
                Arc::from(name),
                Arc::from("(JJJJJJJJ)J"),
                class,
                body,
            );
            cache
                .get("i22l6/C", name, "(JJJJJJJJ)J", class)
                .expect("published")
        };
        let own = put("own", answering(5));
        own.install_not_entrant_stub(record(2, false), fold_args as usize)
            .expect("patch");
        let mut caller = answering(9);
        // Cast: an address, as the compile doors record a baked callee.
        caller._direct_callee_entries = vec![own.entry_ptr() as usize];
        let caller = put("m", caller);
        let own_rec = own.not_entrant.get().expect("patched");
        assert!(!own_rec.forward_to(&caller), "the bare form still refuses");
        let (installed_before, released_before) = cache.cycle_forward_census();
        assert!(
            cache.forward_not_entrant(own_rec, &caller),
            "the cache forwards across the cycle"
        );
        assert!(own_rec
            .forward_target()
            .is_some_and(|t| Arc::ptr_eq(&t, &caller)));
        assert!(
            cache.forward_not_entrant(own_rec, &caller),
            "idempotent once installed"
        );
        assert!(cache.cycle_forward_census().0 > installed_before);

        // A newer body for the record's key supersedes `caller`: the forward
        // lets go of it.
        let newer = put("m", answering(11));
        assert!(own_rec.forward_target().is_none(), "released on supersede");
        assert!(cache.cycle_forward_census().1 > released_before);
        assert!(
            cache.forward_not_entrant(own_rec, &newer),
            "and forwards to the newer body (no cycle now)"
        );
        assert!(own_rec
            .forward_target()
            .is_some_and(|t| Arc::ptr_eq(&t, &newer)));
        for b in [own, caller, newer] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// `whole_cache`: every body is collected, not only the class's own.
    #[test]
    fn a_whole_cache_redefinition_collects_every_body() {
        let cache = JitCache::new();
        let redefined = cratonvm_types::ClassId::new(0x2309);
        let other = cratonvm_types::ClassId::new(0x230A);
        for (class, id) in [("i23l6/Redefined", redefined), ("i23l6/Other", other)] {
            let mut body = seven();
            body.entry_java_args = 0;
            cache.put(Arc::from(class), Arc::from("m"), Arc::from("()I"), id, body);
        }
        let scoped = cache.redefinition_stale_bodies(redefined, "i23l6/Redefined", false);
        assert_eq!(scoped.len(), 1);
        let whole = cache.redefinition_stale_bodies(redefined, "i23l6/Redefined", true);
        assert_eq!(whole.len(), 2);
        assert!(JitCache::is_stale_for_redefinition(
            &scoped[0],
            redefined,
            "i23l6/Redefined"
        ));
        assert_eq!(cache.mark_withdrawn_by_redefinition(&whole), 2);
        assert_eq!(cache.mark_withdrawn_by_redefinition(&whole), 0, "once each");
        assert_eq!(cache.bodies_withdrawn_by_redefinition(), 2);
        for b in scoped.into_iter().chain(whole) {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Interpreter round i1 wave 24, lane L6: a whole-cache redefinition
    /// withdraws every body whose compilation began before its flush, whether
    /// or not anything can reach it (the VM's optimizing OSR body that the
    /// memo already forgot), and no body compiled after it; the answer is
    /// recorded on the body for the exit sinks.
    #[test]
    fn a_whole_cache_redefinition_withdraws_every_body_compiled_before_its_flush() {
        let cache = JitCache::new();
        let before_flush = seven();
        cache.note_whole_cache_redefinition();
        assert!(
            !cache.body_withdrawn_by_redefinition(&before_flush),
            "no flush yet: no barrier"
        );
        let count = cache.bodies_withdrawn_by_redefinition();
        cache.clear_all();
        cache.note_whole_cache_redefinition();
        assert_eq!(
            cache.bodies_withdrawn_by_redefinition(),
            count + 1,
            "the VM's loop-exit handshake trigger moves"
        );
        assert!(
            !before_flush.is_withdrawn_by_redefinition(),
            "never scanned"
        );
        assert!(cache.body_withdrawn_by_redefinition(&before_flush));
        assert!(before_flush.is_withdrawn_by_redefinition(), "and recorded");
        let after_flush = seven();
        assert!(!cache.body_withdrawn_by_redefinition(&after_flush));
    }

    /// Interpreter round i1 wave 24, lane L6: mappings that follow one another
    /// page for page are patched under one protection change; a gap, or a
    /// mapping not starting on a page, ends the run (and on Windows every
    /// mapping is its own run: `VirtualProtect` cannot span two allocations).
    #[test]
    fn adjacent_mappings_are_patched_as_one_run() {
        let p = X86_64_BASE_PAGE;
        let patches = [
            (p, 100),
            (2 * p, p + 1),
            (4 * p, 8),
            (6 * p, 8),
            (7 * p + 8, 8),
        ];
        let runs = adjacent_patch_runs(&patches);
        if cfg!(target_os = "windows") {
            assert_eq!(
                runs,
                vec![
                    (0, 1, p, 2 * p),
                    (1, 2, 2 * p, 4 * p),
                    (2, 3, 4 * p, 5 * p),
                    (3, 4, 6 * p, 7 * p),
                    (4, 5, 7 * p + 8, 8 * p),
                ]
            );
        } else {
            assert_eq!(
                runs,
                vec![
                    (0, 3, p, 5 * p),
                    (3, 4, 6 * p, 7 * p),
                    (4, 5, 7 * p + 8, 8 * p)
                ]
            );
        }
        assert!(adjacent_patch_runs(&[]).is_empty());
    }

    /// `NOP5; MOV R11, flag; TEST BYTE [R11], 0xFF; JNZ +0; MOV EAX, 7; RET`:
    /// a body with one exit poll in the IR tier's outlined form, its site
    /// recorded as both x86-64 tiers record theirs (the condition-code byte,
    /// offset 20).
    fn with_exit_poll(flag: &'static u8) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0x49, 0xBB]);
        // Cast: the flag's address, baked as a 64-bit immediate.
        buf.emit(&(flag as *const u8 as u64).to_le_bytes());
        buf.emit(&[0x41, 0xF6, 0x03, 0xFF]);
        buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
        let mut body = CompiledMethod::new(buf);
        body.entry_java_args = 0;
        body.exit_poll_sites = vec![20];
        // Cast: the flag's address, as the tiers record it.
        body.exit_poll_flag = flag as *const u8 as usize;
        body
    }

    /// Interpreter round i1 wave 26, lane L6: a body superseded while a frame
    /// still runs it (here: held by the test, as a running frame's guard holds
    /// it) is in no map and no baked root, yet a redefinition of a class it
    /// spliced finds it, withdraws it and forces its exit poll; a forcing
    /// that is refused is what keeps the loop-exit retries.
    #[test]
    fn a_superseded_body_a_frame_still_runs_is_found_and_its_polls_forced() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let caller = cratonvm_types::ClassId::new(0x2661);
        let spliced = cratonvm_types::ClassId::new(0x2662);
        let publish = |body: CompiledMethod| {
            cache.put(
                Arc::from("i26l6/Caller"),
                Arc::from("loop"),
                Arc::from("()I"),
                caller,
                body,
            );
            cache.get("i26l6/Caller", "loop", "()I", caller)
        };
        let mut splicing = with_exit_poll(flag);
        splicing.inlined_methods = vec![("i26l6/Spliced".into(), "value".into(), "()I".into())];
        let running = publish(splicing).expect("published");
        // A tier-up publishes a new body under the same key: `running` is
        // superseded, out of every map, and reachable from nothing baked.
        let current = publish(with_exit_poll(flag)).expect("the new body");
        assert!(!Arc::ptr_eq(&running, &current));
        let before = cache.bodies_withdrawn_by_redefinition();
        let stale = cache.redefinition_stale_bodies(spliced, "i26l6/Spliced", false);
        assert!(
            stale.iter().any(|b| Arc::ptr_eq(b, &running)),
            "the superseded body a frame still runs is collected"
        );
        assert!(
            !stale.iter().any(|b| Arc::ptr_eq(b, &current)),
            "the current body did not splice the class"
        );
        assert_eq!(cache.mark_withdrawn_by_redefinition(&stale), 1);
        assert_eq!(
            cache.force_withdrawn_exit_polls(&stale, spliced, "i26l6/Spliced"),
            1
        );
        assert_eq!(running.code_bytes()[20], 0x81, "JNZ became JNO");
        assert_eq!(
            current.code_bytes()[20],
            0x85,
            "the current body's poll is untouched"
        );
        assert!(cache.every_withdrawal_forced_since(before));

        // A withdrawn body whose recorded site is not a poll: nothing is
        // written, and the retries stay.
        let mut odd = with_exit_poll(flag);
        odd.exit_poll_sites = vec![3];
        odd.inlined_methods = vec![("i26l6/Spliced".into(), "value".into(), "()I".into())];
        let odd = Arc::new(odd);
        let before = cache.bodies_withdrawn_by_redefinition();
        assert_eq!(cache.mark_withdrawn_by_redefinition(&[Arc::clone(&odd)]), 1);
        assert_eq!(
            cache.force_withdrawn_exit_polls(&[Arc::clone(&odd)], spliced, "i26l6/Spliced"),
            0
        );
        assert!(!cache.every_withdrawal_forced_since(before));
        assert!(
            cache.every_withdrawal_forced_since(cache.bodies_withdrawn_by_redefinition() + 1),
            "a later snapshot is past it"
        );
        for b in stale.into_iter().chain([running, current, odd]) {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Interpreter round i1 wave 27, lane L6: a body its own class's
    /// redefinition spared (an obsolete activation, whose verdict is "stay"
    /// for good) is not forced by a later redefinition of another class
    /// either, while a body that only spliced that other class still is.
    #[test]
    fn a_body_spared_as_obsolete_is_not_forced_by_a_later_redefinition() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let own = cratonvm_types::ClassId::new(0x2771);
        let other = cratonvm_types::ClassId::new(0x2772);
        let mut obsolete = with_exit_poll(flag);
        obsolete.owner_class_id = own.as_u32();
        let obsolete = Arc::new(obsolete);
        let mut splicing = with_exit_poll(flag);
        splicing.inlined_methods = vec![("i27l6/Other".into(), "value".into(), "()I".into())];
        let splicing = Arc::new(splicing);
        let first = [Arc::clone(&obsolete)];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&first), 1);
        assert_eq!(
            cache.force_withdrawn_exit_polls(&first, own, "i27l6/Own"),
            0,
            "its own class's redefinition spares it"
        );
        assert_eq!(obsolete.code_bytes()[20], 0x85);
        // A later (whole-cache) redefinition of another class lists both.
        let second = [Arc::clone(&obsolete), Arc::clone(&splicing)];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&second), 1);
        assert_eq!(
            cache.force_withdrawn_exit_polls(&second, other, "i27l6/Other"),
            1,
            "only the splicing body is forced"
        );
        assert_eq!(obsolete.code_bytes()[20], 0x85, "still spared");
        assert_eq!(splicing.code_bytes()[20], 0x81, "JNZ became JNO");
        drop((first, second));
        crate::defer_jit_owner(Some(obsolete));
        crate::defer_jit_owner(Some(splicing));
    }

    /// Interpreter round i1 wave 45, lane L2: after a redefinition that
    /// renumbered its class's pool, an OSR body of the class's own bytecode is
    /// marked and forced (with `OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE` on),
    /// while a method-entry body of the class, and an OSR body after a
    /// redefinition that moved no constant, stay spared as before.
    #[test]
    fn an_own_class_osr_body_leaves_after_a_renumbering_redefinition() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let own = cratonvm_types::ClassId::new(0x4521);
        let own_body = |osr: bool| {
            let mut body = with_exit_poll(flag);
            body.owner_class_id = own.as_u32();
            body.compiled_via_osr = osr;
            Arc::new(body)
        };
        let loop_body = own_body(true);
        let entry_body = own_body(false);
        let kept_body = own_body(true);
        let renumbering = [Arc::clone(&loop_body), Arc::clone(&entry_body)];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&renumbering), 2);
        let forced = cache.force_withdrawn_exit_polls_after(&renumbering, own, "i45l2/Own", true);
        if OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE {
            assert_eq!(forced, 1, "the OSR body only");
            assert!(loop_body.leaves_as_renumbered_obsolete());
            assert_eq!(loop_body.code_bytes()[20], 0x81, "JNZ became JNO");
        } else {
            assert_eq!(forced, 0, "switched off: spared as in wave 44");
            assert!(!loop_body.leaves_as_renumbered_obsolete());
            assert_eq!(loop_body.code_bytes()[20], 0x85);
        }
        assert!(!entry_body.leaves_as_renumbered_obsolete());
        assert_eq!(
            entry_body.code_bytes()[20],
            0x85,
            "a method-entry body stays spared"
        );
        // A redefinition that moved no constant spares the OSR body too.
        let constant_only = [Arc::clone(&kept_body)];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&constant_only), 1);
        assert_eq!(
            cache.force_withdrawn_exit_polls_after(&constant_only, own, "i45l2/Own", false),
            0
        );
        assert!(!kept_body.leaves_as_renumbered_obsolete());
        assert_eq!(kept_body.code_bytes()[20], 0x85);
        drop((renumbering, constant_only));
        crate::defer_jit_owner(Some(loop_body));
        crate::defer_jit_owner(Some(entry_body));
        crate::defer_jit_owner(Some(kept_body));
    }

    /// Interpreter round i1 wave 46, lane L2: after a redefinition that
    /// renumbered its class's pool, a METHOD-ENTRY body of the class's own
    /// bytecode is marked and forced (with `OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`
    /// on) only when a stash sink can rebuild its frame in its own bytecode:
    /// a compiled source and a pool stamp, not `ACC_SYNCHRONIZED`, no monitor
    /// in its code. The VM's verdict memo starts unasked.
    #[test]
    fn an_own_class_entry_body_leaves_only_when_a_sink_can_rebuild_it() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let own = cratonvm_types::ClassId::new(0x4621);
        let source = |code: &[u8], is_synchronized: bool| {
            Arc::new(crate::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: own,
                    class_name: Arc::from("i46l2/Own"),
                    method_name: Arc::from("spin"),
                    method_descriptor: Arc::from("()V"),
                    source_file: None,
                    code: Arc::from(code),
                    exception_table: Arc::from(Vec::new().as_slice()),
                    max_stack: 1,
                    max_locals: 1,
                    num_params: 0,
                    is_synchronized,
                    is_static: true,
                },
            ))
        };
        let entry_body = |src: Option<Arc<crate::CachedBytecodeMethod>>| {
            let mut body = with_exit_poll(flag);
            body.owner_class_id = own.as_u32();
            body.compile_cp_stamp = Some(1);
            if let Some(src) = src {
                assert!(body.stamp_compiled_source(src));
            }
            Arc::new(body)
        };
        // `return`, padded as the VM pads bytecode.
        let plain = entry_body(Some(source(&[0xb1, 0, 0], false)));
        let synchronized = entry_body(Some(source(&[0xb1, 0, 0], true)));
        // `aload_0; monitorenter; return`.
        let locking = entry_body(Some(source(&[0x2a, 0xc2, 0xb1, 0, 0], false)));
        let unstamped = entry_body(None);
        let bodies = [
            Arc::clone(&plain),
            Arc::clone(&synchronized),
            Arc::clone(&locking),
            Arc::clone(&unstamped),
        ];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&bodies), 4);
        let forced = cache.force_withdrawn_exit_polls_after(&bodies, own, "i46l2/Own", true);
        if OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE {
            assert_eq!(forced, 1, "the resumable body only");
            assert!(plain.leaves_as_renumbered_obsolete_entry());
            assert_eq!(plain.renumbered_entry_resumes(), None, "not asked yet");
            assert_eq!(plain.code_bytes()[20], 0x81, "JNZ became JNO");
            plain.note_renumbered_entry_resumes(true);
            assert_eq!(plain.renumbered_entry_resumes(), Some(true));
        } else {
            assert_eq!(forced, 0, "switched off: spared as in wave 45");
            assert!(!plain.leaves_as_renumbered_obsolete());
            assert_eq!(plain.code_bytes()[20], 0x85);
        }
        for spared in [&synchronized, &locking, &unstamped] {
            assert!(!spared.leaves_as_renumbered_obsolete());
            assert_eq!(spared.code_bytes()[20], 0x85, "no sink could rebuild it");
        }
        drop(bodies);
        crate::defer_jit_owner(Some(plain));
        crate::defer_jit_owner(Some(synchronized));
        crate::defer_jit_owner(Some(locking));
        crate::defer_jit_owner(Some(unstamped));
    }

    /// Interpreter round i1 wave 47, lane L2: bodies a redefinition that moved
    /// no constant spared as their class's obsolete activations stay spared
    /// by a later redefinition of another class, but an interpreter-only
    /// withdrawal forces the ones whose exit can be resumed (with
    /// `INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE` on): an OSR body and a
    /// method-entry body a sink can rebuild, not a synchronized one. The
    /// verdict memo is keyed by the redefinition count.
    #[test]
    fn an_interpreter_only_withdrawal_forces_resumable_obsolete_bodies() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let own = cratonvm_types::ClassId::new(0x4721);
        let other = cratonvm_types::ClassId::new(0x4722);
        let source = |is_synchronized: bool| {
            Arc::new(crate::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: own,
                    class_name: Arc::from("i47l2/Own"),
                    method_name: Arc::from("spin"),
                    method_descriptor: Arc::from("()V"),
                    source_file: None,
                    code: Arc::from([0xb1u8, 0, 0].as_slice()),
                    exception_table: Arc::from(Vec::new().as_slice()),
                    max_stack: 1,
                    max_locals: 1,
                    num_params: 0,
                    is_synchronized,
                    is_static: true,
                },
            ))
        };
        let own_body = |osr: bool, src: Option<Arc<crate::CachedBytecodeMethod>>| {
            let mut body = with_exit_poll(flag);
            body.owner_class_id = own.as_u32();
            body.compiled_via_osr = osr;
            body.compile_cp_stamp = Some(1);
            if let Some(src) = src {
                assert!(body.stamp_compiled_source(src));
            }
            Arc::new(body)
        };
        let loop_body = own_body(true, None);
        let entry_body = own_body(false, Some(source(false)));
        let synchronized = own_body(false, Some(source(true)));
        let bodies = [
            Arc::clone(&loop_body),
            Arc::clone(&entry_body),
            Arc::clone(&synchronized),
        ];
        assert_eq!(cache.mark_withdrawn_by_redefinition(&bodies), 3);
        assert_eq!(
            cache.force_withdrawn_exit_polls_after(&bodies, own, "i47l2/Own", false),
            0,
            "its own class's constant-only redefinition spares every one"
        );
        assert_eq!(
            cache.force_withdrawn_exit_polls(&bodies, other, "i47l2/Other"),
            0,
            "a redefinition of another class keeps the spare"
        );
        let forced = cache.force_exit_polls_for_the_interpreter(&bodies, "<interpreter-only>");
        // The synchronized body leaves too once the interpreter resumes a
        // locking activation (`INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE`).
        let locking = INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE
            && INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE;
        if INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE {
            assert_eq!(
                forced,
                2 + usize::from(locking),
                "the OSR body and the resumable method-entry bodies"
            );
            assert_eq!(loop_body.code_bytes()[20], 0x81, "JNZ became JNO");
            assert_eq!(entry_body.code_bytes()[20], 0x81, "JNZ became JNO");
        } else {
            assert_eq!(forced, 0, "switched off: spared as in wave 46");
            assert_eq!(loop_body.code_bytes()[20], 0x85);
            assert_eq!(entry_body.code_bytes()[20], 0x85);
        }
        assert_eq!(
            synchronized.code_bytes()[20],
            if locking { 0x81 } else { 0x85 },
            "rebuilt only for a locking resume"
        );
        assert!(
            !entry_body.leaves_as_renumbered_obsolete(),
            "no renumbering mark"
        );
        assert_eq!(entry_body.obsolete_entry_resumes_at(7), None, "never asked");
        entry_body.note_obsolete_entry_resumes_at(7, true);
        assert_eq!(entry_body.obsolete_entry_resumes_at(7), Some(true));
        assert_eq!(
            entry_body.obsolete_entry_resumes_at(8),
            None,
            "a later redefinition voids it"
        );
        entry_body.note_obsolete_entry_resumes_at(8, false);
        assert_eq!(entry_body.obsolete_entry_resumes_at(8), Some(false));
        drop(bodies);
        crate::defer_jit_owner(Some(loop_body));
        crate::defer_jit_owner(Some(entry_body));
        crate::defer_jit_owner(Some(synchronized));
    }

    /// Interpreter round i1 wave 26, lane L6: a body the cache never published
    /// (the VM's optimizing OSR body) is listed for as long as it lives, and a
    /// body with no exit poll is not listed at all.
    #[test]
    fn an_unpublished_exit_poll_body_is_listed_while_it_lives() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let osr = Arc::new(with_exit_poll(flag));
        let pollless = Arc::new(seven());
        cache.note_exit_poll_body(&osr);
        cache.note_exit_poll_body(&pollless);
        let listed = cache.live_unpublished_exit_poll_bodies();
        assert_eq!(listed.len(), 1);
        assert!(Arc::ptr_eq(&listed[0], &osr));
        drop(listed);
        drop(osr);
        assert!(cache.live_unpublished_exit_poll_bodies().is_empty());
        drop(pollless);
    }

    /// Interpreter round i1 wave 24, lane L6: once a redefinition fenced
    /// publications, a compile that began before it (the old bytecode) cannot
    /// publish -- so it cannot slip in between the not-entrant scan and the
    /// eviction -- and one that began after it can.
    #[test]
    fn a_fenced_redefinition_refuses_the_old_bytecodes_compiles() {
        let cache = JitCache::new();
        let put = |class: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(Arc::from(class), Arc::from("m"), Arc::from("()I"), id, body);
            cache.get(class, "m", "()I", id)
        };
        for (class, id, whole_cache) in [
            ("i24l6/Scoped", cratonvm_types::ClassId::new(0x2441), false),
            ("i24l6/Whole", cratonvm_types::ClassId::new(0x2442), true),
        ] {
            let old = seven();
            cache.fence_redefinition_publications(id, class, whole_cache);
            assert!(
                put(class, id, old).is_none(),
                "{class}: begun before the fence, refused"
            );
            let fresh = put(class, id, seven());
            assert!(fresh.is_some(), "{class}: begun after it, published");
            crate::defer_jit_owner(fresh);
        }
    }

    /// The VM helper's stand-in for the supersede-redirect tests: a call the
    /// stub could not forward answers 4242.
    extern "C" fn w16_slow(_record: i64, _regs: i64, _stack: i64) -> i64 {
        4242
    }

    /// nio21 wave 16 (lane L1): an armed cache makes the body a publication
    /// displaces not entrant -- a single-pass body displaced by another
    /// single-pass body too -- and the displaced stub forwards to the current
    /// body; an optimizing body displaced by a single-pass one is left alone.
    #[test]
    fn w16l1_a_displaced_body_is_redirected_to_its_successor() {
        use std::sync::atomic::Ordering;
        let cache = JitCache::new();
        if !cache.supersede_redirect {
            return; // `CRATONVM_JIT_SUPERSEDE_REDIRECT=0` in this environment.
        }
        cache.arm_not_entrant(NotEntrantHook {
            helper: w16_slow as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x1601);
        let put = |mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from("w16l1/C"),
                Arc::from("m"),
                Arc::from("()I"),
                class,
                body,
            );
            cache.get("w16l1/C", "m", "()I", class).expect("published")
        };
        let first = put(answering(7));
        assert!(
            !first.is_not_entrant(),
            "a first publication displaces nothing"
        );
        let second = put(answering(9));
        assert!(first.is_not_entrant(), "C1 displaced by C1: redirected");
        assert!(first.retired.load(Ordering::Acquire));
        assert!(!second.is_not_entrant());
        assert_eq!(cache.supersede_redirect_counts(), (1, 0));
        // SAFETY: the entry jumps to the stub; no forward yet: the helper.
        assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 4242);
        let rec = first.not_entrant.get().expect("patched");
        assert!(cache.forward_not_entrant(rec, &second));
        // SAFETY: the stub forwards to `second`: MOV EAX, 9; RET.
        assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 9);

        let mut optimizing = answering(11);
        optimizing.used_ir_backend = true;
        let optimizing = put(optimizing);
        assert!(second.is_not_entrant(), "C1 displaced by C2: redirected");
        // SAFETY: `first` forwards to `second`, superseded now: the slow
        // path, where the VM would re-forward to the current body.
        assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 4242);
        assert!(cache.forward_not_entrant(rec, &optimizing));
        // SAFETY: re-forwarded: MOV EAX, 11; RET.
        assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 11);

        let single = put(answering(13));
        assert!(
            !optimizing.is_not_entrant(),
            "C2 displaced by C1: its baked callers keep the optimizing code"
        );
        assert_eq!(cache.supersede_redirect_counts(), (2, 0));
        for b in [first, second, optimizing, single] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// nio21 wave 16 (lane L1): a body displaced before the cache was armed
    /// waits, and the next publication of its key once armed redirects it
    /// with the body that publication displaces; another key's waiting body
    /// is untouched.
    #[test]
    fn w16l1_a_body_displaced_before_arming_is_redirected_at_the_next_publication() {
        let cache = JitCache::new();
        if !cache.supersede_redirect {
            return;
        }
        let class = cratonvm_types::ClassId::new(0x1602);
        let put = |name: &str, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from("w16l1/D"),
                Arc::from(name),
                Arc::from("()I"),
                class,
                body,
            );
            cache.get("w16l1/D", name, "()I", class).expect("published")
        };
        let a = put("m", answering(7));
        let b = put("m", answering(9));
        let other_old = put("n", answering(5));
        let other_new = put("n", answering(6));
        assert!(!a.is_not_entrant(), "not armed: nothing patched");
        cache.arm_not_entrant(NotEntrantHook {
            helper: w16_slow as usize,
            vm_ptr: 0,
        });
        let c = put("m", answering(11));
        assert!(a.is_not_entrant(), "the waiting body");
        assert!(b.is_not_entrant(), "the body this publication displaced");
        assert!(!c.is_not_entrant());
        assert!(!other_old.is_not_entrant(), "another key waits for its own");
        // SAFETY: no forward yet: the helper.
        assert_eq!(unsafe { a.try_call(&[]) }.expect("call"), 4242);
        for x in [a, b, c, other_old, other_new] {
            crate::defer_jit_owner(Some(x));
        }
    }

    /// STASH20-2: `NOP5; SUB RSP, 0x28; NOP x nops; CALL rel32 target;
    /// ADD RSP, 0x28; RET` -- a caller that baked a direct call to `target`
    /// (16-aligned at the call, with the Win64 shadow space), and the offset
    /// of the call's return address. `None` when `target` is out of `rel32`
    /// reach of this buffer.
    fn calling(target: usize, nops: usize) -> Option<(CompiledMethod, usize)> {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0x48, 0x83, 0xEC, 0x28]);
        for _ in 0..nops {
            buf.emit(&[0x90]);
        }
        let ret_off = buf.pos() + 5;
        // Cast: addresses, for the displacement.
        let ret_pc = buf.as_ptr() as usize + ret_off;
        let rel = i32::try_from(target as i64 - ret_pc as i64).ok()?;
        buf.emit(&[0xE8]);
        buf.emit(&rel.to_le_bytes());
        buf.emit(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        Some((CompiledMethod::new(buf), ret_off))
    }

    /// JIT r23 wave 10, lane L6: `calling`'s twin with the optimizing tier's
    /// `MOV RAX, imm64 ; CALL RAX`, the imm64 on an 8-byte boundary when
    /// `aligned`, one byte past it otherwise. Answers the return offset.
    fn calling_abs64(target: usize, aligned: bool) -> (CompiledMethod, usize) {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0x48, 0x83, 0xEC, 0x28]);
        // Cast: an address, for its alignment only.
        let at = buf.as_ptr() as usize + buf.pos();
        let mut pad = crate::ir_lower::direct_call_imm_align_pad(at);
        if !aligned {
            pad = (pad + 1) % 8;
        }
        for _ in 0..pad {
            buf.emit(&[0x90]);
        }
        buf.emit(&[0x48, 0xB8]);
        buf.emit(&(target as u64).to_le_bytes());
        buf.emit(&[0xFF, 0xD0]);
        let ret_off = buf.pos();
        buf.emit(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        (CompiledMethod::new(buf), ret_off)
    }

    /// JIT r23 wave 10, lane L6 (`CRATONVM_JIT_STALE_CALL_REBIND_ABS64`): a
    /// caller whose direct call is `MOV RAX, imm64 ; CALL RAX` with an aligned
    /// imm64 is rebound in place; a misaligned one, and any with the switch
    /// off, is refused and keeps forwarding.
    #[test]
    fn r23w10_an_aligned_abs64_stale_call_is_rebound() {
        let cache = JitCache::new();
        if !cache.supersede_redirect {
            return;
        }
        cache.arm_not_entrant(NotEntrantHook {
            helper: w16_slow as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x2310);
        let callers = cratonvm_types::ClassId::new(0x2311);
        let put = |class_name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from(class_name),
                Arc::from("m"),
                Arc::from("()I"),
                id,
                body,
            );
            cache.get(class_name, "m", "()I", id).expect("published")
        };
        let old = put("w10/T", class, answering(7));
        // Cast: an address, as the compile doors record a baked callee.
        let old_entry = old.entry_ptr() as usize;
        let (mut caller, ret_off) = calling_abs64(old_entry, true);
        let (mut skewed, s_off) = calling_abs64(old_entry, false);
        caller._direct_callee_entries = vec![old_entry];
        skewed._direct_callee_entries = vec![old_entry];
        let caller = put("w10/Caller", callers, caller);
        let skewed = put("w10/Skewed", callers, skewed);
        // Cast: addresses, the return sites.
        let ret_pc = caller.entry_ptr() as usize + ret_off;
        let s_ret = skewed.entry_ptr() as usize + s_off;
        // SAFETY: the caller calls `old`: MOV EAX, 7; RET.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 7);
        let new = put("w10/T", class, answering(9));
        let rec = old.not_entrant.get().expect("patched");
        assert!(cache.forward_not_entrant(rec, &new));
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &skewed, s_ret),
            Err("the call's imm64 is not 8-byte aligned")
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_STALE_CALL_REBIND_ABS64", Some("0"))],
            || {
                assert_eq!(
                    cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
                    Err("the site is MOV RAX, imm64 ; CALL RAX and the abs64 rebind is off")
                );
            },
        );
        assert!(!caller.has_rebound_call_sites(), "a refusal marks nothing");
        assert!(cache.rebind_stale_call_site(rec, &new, &caller, ret_pc).is_ok());
        assert!(caller.has_rebound_call_sites());
        // SAFETY: the caller's code is mapped and readable while it lives.
        let imm = unsafe { ((ret_pc - 10) as *const u64).read() };
        // Cast: an address, compared.
        assert_eq!(imm as usize, new.entry_ptr() as usize);
        // SAFETY: the caller calls `new` directly now.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 9);
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
            Err("the return site's CALL does not name the patched body"),
            "rebound once"
        );
        for b in [old, new, caller, skewed] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// STASH20-2 (JIT round 20 wave 19, lane yStash): a caller that baked the
    /// displaced body's entry is re-pointed at the current body once the
    /// stub forwards there, atomically, and only a caller that roots the
    /// displaced body, at a direct `CALL rel32` of it whose displacement lies
    /// in one 8-byte word. The rebind clears the forward word for the next
    /// stale caller, whose slow call publishes the same kept forward again.
    #[test]
    fn ystash_a_stale_direct_call_is_rebound_to_the_current_body() {
        let cache = JitCache::new();
        if !cache.supersede_redirect {
            return; // `CRATONVM_JIT_SUPERSEDE_REDIRECT=0` in this environment.
        }
        cache.arm_not_entrant(NotEntrantHook {
            helper: w16_slow as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x2019);
        let callers = cratonvm_types::ClassId::new(0x201A);
        let put = |class_name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from(class_name),
                Arc::from("m"),
                Arc::from("()I"),
                id,
                body,
            );
            cache.get(class_name, "m", "()I", id).expect("published")
        };
        let old = put("ys/T", class, answering(7));
        // Cast: an address, as the compile doors record a baked callee.
        let old_entry = old.entry_ptr() as usize;
        let built = (
            calling(old_entry, 0),
            calling(old_entry, 3),
            calling(old_entry, 0),
        );
        let (Some((mut caller, ret_off)), Some((mut straddling, s_off)), Some((unrooted, u_off))) =
            built
        else {
            crate::defer_jit_owner(Some(old));
            return; // The buffers landed out of `rel32` reach of each other.
        };
        caller._direct_callee_entries = vec![old_entry];
        straddling._direct_callee_entries = vec![old_entry];
        let caller = put("ys/Caller", callers, caller);
        let straddling = put("ys/Straddling", callers, straddling);
        let unrooted = put("ys/Unrooted", callers, unrooted);
        // Cast: addresses, the return sites of the three calls.
        let ret_pc = caller.entry_ptr() as usize + ret_off;
        let s_ret = straddling.entry_ptr() as usize + s_off;
        let u_ret = unrooted.entry_ptr() as usize + u_off;
        // SAFETY: the caller calls `old`: MOV EAX, 7; RET.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 7);

        let new = put("ys/T", class, answering(9));
        // Cast: addresses, for the reach test.
        if i32::try_from(new.entry_ptr() as i64 - ret_pc as i64).is_err() {
            for b in [old, new, caller, straddling, unrooted] {
                crate::defer_jit_owner(Some(b));
            }
            return; // The new body landed out of `rel32` reach of the caller.
        }
        assert!(old.is_not_entrant(), "C1 displaced by C1: redirected");
        let rec = old.not_entrant.get().expect("patched");
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
            Err("no forward of the stub owns the current body"),
            "nothing to rebind to before the VM installs the forward"
        );
        // SAFETY: the stub has no forward yet: the helper answers.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 4242);
        assert!(cache.forward_not_entrant(rec, &new));
        // SAFETY: the stub forwards to `new`: MOV EAX, 9; RET.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 9);

        // Cast: an address inside the caller that ends no CALL.
        let not_a_call = caller.entry_ptr() as usize + 5;
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &caller, not_a_call),
            Err("the return site is not a direct CALL rel32")
        );
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &straddling, s_ret),
            Err("the call's displacement straddles an 8-byte word")
        );
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &unrooted, u_ret),
            Err("the caller does not root the patched body")
        );
        assert!(!caller.has_rebound_call_sites(), "a refusal marks nothing");

        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
            Ok(true)
        );
        assert!(caller.has_rebound_call_sites());
        // SAFETY: the caller's code is mapped and readable while it lives.
        let disp = unsafe { ((ret_pc - 4) as *const i32).read_unaligned() };
        // Cast: an address and a sign-extended displacement.
        assert_eq!(
            ret_pc.wrapping_add(disp as isize as usize),
            new.entry_ptr() as usize
        );
        // SAFETY: the caller calls `new` directly now.
        assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 9);
        assert_eq!(
            cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
            Err("the return site's CALL does not name the patched body"),
            "rebound once"
        );
        // The word was cleared: the next stale call goes through the VM once,
        // and its forward re-publishes the kept box instead of a new one.
        // SAFETY: the stub's forward word is clear: the helper answers.
        assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 4242);
        assert!(cache.forward_not_entrant(rec, &new));
        assert_eq!(
            rec.forwards.lock().len(),
            1,
            "the kept forward, re-published"
        );
        // SAFETY: forwarded to `new` again.
        assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 9);
        assert!(
            rec.rebinds.load(std::sync::atomic::Ordering::Relaxed) == 1,
            "one site rebound: no cycle forward may be installed for this record any more"
        );
        // Lane rev16: the rebound caller bakes `new` now, so withdrawing `new`
        // withdraws the caller too (the forward it skips tested `retired`).
        assert!(cache.get("ys/Straddling", "m", "()I", callers).is_some());
        cache.remove("ys/T", "m", "()I", class);
        assert!(new.retired.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            cache.get("ys/Caller", "m", "()I", callers).is_none(),
            "a caller rebound at a withdrawn body is withdrawn with it"
        );
        assert!(
            cache.get("ys/Straddling", "m", "()I", callers).is_some(),
            "a caller that still calls through the stub keeps its body"
        );
        for b in [old, new, caller, straddling, unrooted] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// STASH20-1's stand-in for the VM's tiered manager: the method `cyc` is a
    /// sticky recursion-cycle member.
    struct CycleSink;

    impl crate::JitBodyWithdrawalSink for CycleSink {
        fn bodies_withdrawn(&self, _keys: &[crate::WithdrawnMethodKey]) {}

        fn forward_must_dispatch(
            &self,
            _class_name: &str,
            method_name: &str,
            _descriptor: &str,
            _class_id: u32,
        ) -> bool {
            method_name == "cyc"
        }
    }

    /// STASH20-1 (JIT round 20 wave 19, lane yStash): the displaced body of a
    /// method whose forward the VM would refuse stays entrant (its baked
    /// callers keep running it, not a by-name re-dispatch per call); another
    /// method's displaced body is redirected as before.
    #[test]
    fn ystash_a_cycle_members_displaced_body_stays_entrant() {
        use std::sync::atomic::Ordering;
        let cache = JitCache::new();
        if !cache.supersede_redirect || !supersede_skip_refused_forward_enabled() {
            return; // A kill switch is set in this environment.
        }
        let sink: Arc<dyn crate::JitBodyWithdrawalSink> = Arc::new(CycleSink);
        if !cache.install_withdrawal_sink(Arc::downgrade(&sink)) {
            return; // `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0`: no sink, no verdict.
        }
        cache.arm_not_entrant(NotEntrantHook {
            helper: w16_slow as usize,
            vm_ptr: 0,
        });
        let class = cratonvm_types::ClassId::new(0x201B);
        let put = |name: &str, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from("ys/Cycle"),
                Arc::from(name),
                Arc::from("()I"),
                class,
                body,
            );
            cache
                .get("ys/Cycle", name, "()I", class)
                .expect("published")
        };
        let cyc_old = put("cyc", answering(7));
        let cyc_new = put("cyc", answering(9));
        assert!(
            !cyc_old.is_not_entrant(),
            "the VM would refuse the forward: left entrant"
        );
        assert!(!cyc_old.retired.load(Ordering::Acquire));
        // SAFETY: unpatched: MOV EAX, 7; RET.
        assert_eq!(unsafe { cyc_old.try_call(&[]) }.expect("call"), 7);
        let m_old = put("m", answering(5));
        let m_new = put("m", answering(6));
        assert!(
            m_old.is_not_entrant(),
            "a method the VM forwards is redirected"
        );
        assert_eq!(cache.supersede_redirect_counts(), (1, 0));
        for b in [cyc_old, cyc_new, m_old, m_new] {
            crate::defer_jit_owner(Some(b));
        }
        drop(sink);
    }

    /// JIT round 23 wave 1, lane L1 (THROWS22-1, `CRATONVM_JIT_OSR_PROFIT_EXIT`):
    /// arming a single-pass OSR body for a profit exit forces its exit polls
    /// once, sets the fact the poll verdict asks, and leaves
    /// `exit_polls_forced` alone, so a later redefinition's force pass still
    /// accepts the (already forced) sites and marks the body. A method-entry
    /// body and a withdrawn body are never armed or told to leave for profit.
    #[test]
    fn r23_l1_an_armed_single_pass_osr_body_has_its_polls_forced_once() {
        use std::sync::atomic::Ordering;
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        if !cache.osr_profit_exit_enabled() {
            // `CRATONVM_JIT_OSR_PROFIT_EXIT=0` in the test environment.
            let mut b = with_exit_poll(flag);
            b.compiled_via_osr = true;
            assert_eq!(cache.arm_osr_profit_exit(&Arc::new(b)), Err("switch-off"));
            return;
        }
        let entry = Arc::new(with_exit_poll(flag));
        assert_eq!(cache.arm_osr_profit_exit(&entry), Err("not-an-osr-body"));
        // An IR OSR body that records exit polls is armable (the late-bind OSR
        // exit); one with none is refused like any other.
        let mut ir = with_exit_poll(flag);
        ir.compiled_via_osr = true;
        ir.used_ir_backend = true;
        ir.exit_poll_sites.clear();
        assert_eq!(cache.arm_osr_profit_exit(&Arc::new(ir)), Err("no-exit-polls"));
        let mut bare = with_exit_poll(flag);
        bare.compiled_via_osr = true;
        bare.exit_poll_sites.clear();
        assert_eq!(cache.arm_osr_profit_exit(&Arc::new(bare)), Err("no-exit-polls"));
        assert_eq!(cache.osr_profit_exits_armed(), 0, "no refusal arms");

        let mut b = with_exit_poll(flag);
        b.compiled_via_osr = true;
        let body = Arc::new(b);
        assert!(!body.osr_profit_exit_armed());
        assert!(!cache.osr_profit_exit_may_leave(&body), "not armed yet");
        assert_eq!(body.code_bytes()[20], 0x85);
        assert_eq!(cache.arm_osr_profit_exit(&body), Ok(1));
        assert_eq!(body.code_bytes()[20], 0x81, "JNZ became JNO");
        assert!(body.osr_profit_exit_armed());
        assert!(cache.osr_profit_exit_may_leave(&body));
        assert_eq!(cache.osr_profit_exits_armed(), 1);
        assert_eq!(cache.arm_osr_profit_exit(&body), Ok(0), "armed once");
        assert_eq!(cache.osr_profit_exits_armed(), 1);

        // A later redefinition still forces the body (same byte) and marks it.
        assert!(!body.exit_polls_forced.load(Ordering::Acquire));
        assert_eq!(body.force_exit_polls(), Ok(1));
        assert_eq!(body.code_bytes()[20], 0x81);
        assert!(body.exit_polls_forced.load(Ordering::Acquire));

        // Withdrawn: the redefinition owns its exits now.
        assert_eq!(cache.mark_withdrawn_by_redefinition(&[Arc::clone(&body)]), 1);
        assert!(!cache.osr_profit_exit_may_leave(&body));
        let mut late = with_exit_poll(flag);
        late.compiled_via_osr = true;
        late.withdrawn_by_redefinition
            .store(true, Ordering::Release);
        assert_eq!(
            cache.arm_osr_profit_exit(&Arc::new(late)),
            Err("redefinition-owns-its-exits")
        );

        assert_eq!(cache.note_osr_profit_exit_verdict(), 1);
        assert_eq!(cache.note_osr_profit_exit_taken(), 1);
        assert_eq!(cache.osr_profit_exit_counts(), (1, 1, 1));
        crate::defer_jit_owner(Some(body));
        crate::defer_jit_owner(Some(entry));
    }

    /// JIT round 23 wave 1, lane L1: the exit-poll shape check answers a poll
    /// forced already with its own byte, so the redefinition's pass does not
    /// refuse a body the OSR profit exit forced first.
    #[test]
    fn r23_l1_a_forced_poll_is_still_a_well_shaped_poll() {
        let flag = 0x7000_0000usize;
        let at = 0x1000usize;
        let sp = [0x41, 0xF6, 0x43, 0x00, 0xFF, 0x0F, 0x80, 0, 0, 0, 0];
        assert_eq!(forced_exit_poll_cc(&sp, at, 6, flag), Some(0x80));
        let ir = [0x41, 0xF6, 0x03, 0xFF, 0x0F, 0x81, 0, 0, 0, 0];
        assert_eq!(forced_exit_poll_cc(&ir, at, 5, flag), Some(0x81));
        // Any other condition code is still refused.
        let jl = [0x41, 0xF6, 0x03, 0xFF, 0x0F, 0x8C, 0, 0, 0, 0];
        assert_eq!(forced_exit_poll_cc(&jl, at, 5, flag), None);
    }

    /// Review item 1 of `r23-w1-review-osr-profit-exit-FIXED-20261021.md`: a body
    /// whose profit exit cannot be taken is disarmed, its polls written back
    /// and never armed again; a redefinition's force stands over a disarm.
    #[test]
    fn r23_l1_a_disarmed_body_gets_its_polls_back_unless_a_redefinition_forced_them() {
        use std::sync::atomic::Ordering;
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        if !cache.osr_profit_exit_enabled() {
            return;
        }
        let mut b = with_exit_poll(flag);
        b.compiled_via_osr = true;
        let body = Arc::new(b);
        assert_eq!(cache.disarm_osr_profit_exit(&body), 0, "not armed: nothing");
        assert_eq!(cache.arm_osr_profit_exit(&body), Ok(1));
        assert_eq!(body.code_bytes()[20], 0x81);
        assert_eq!(cache.disarm_osr_profit_exit(&body), 1);
        assert_eq!(body.code_bytes()[20], 0x85, "JNO back to JNZ");
        assert!(!body.osr_profit_exit_armed());
        assert!(!cache.osr_profit_exit_may_leave(&body));
        assert_eq!(cache.osr_profit_exit_disarms(), 1);
        assert_eq!(cache.arm_osr_profit_exit(&body), Err("disarmed-before"));
        assert_eq!(cache.disarm_osr_profit_exit(&body), 0, "once");

        let mut f = with_exit_poll(flag);
        f.compiled_via_osr = true;
        let forced = Arc::new(f);
        assert_eq!(cache.arm_osr_profit_exit(&forced), Ok(1));
        assert_eq!(forced.force_exit_polls(), Ok(1), "a redefinition forces it too");
        assert!(forced.exit_polls_forced.load(Ordering::Acquire));
        assert_eq!(cache.disarm_osr_profit_exit(&forced), 0);
        assert_eq!(forced.code_bytes()[20], 0x81, "the redefinition's force stands");
        assert!(!forced.osr_profit_exit_armed());
        assert_eq!(cache.osr_profit_exit_disarms(), 2);
    }

    /// t1 lane jitea (L1W1-2): the single-pass OSR bodies a later `put_osr`
    /// superseded while a frame still runs them are listed for the profit
    /// arm -- same owner class and label only, never the current body, an IR
    /// body, another method, or a body armed already.
    #[test]
    fn t1_jitea_superseded_single_pass_osr_bodies_are_listed() {
        let flag: &'static u8 = Box::leak(Box::new(0u8));
        let cache = JitCache::new();
        let make = |label: &str, owner: u32, ir: bool| {
            let mut b = with_exit_poll(flag);
            b.compiled_via_osr = true;
            b.used_ir_backend = ir;
            b.method_label = label.to_string();
            b.owner_class_id = owner;
            let body = Arc::new(b);
            cache.note_exit_poll_body(&body);
            body
        };
        let label = "t1/Loop.run:()V";
        let current = make(label, 7, false);
        let old = make(label, 7, false);
        let other_owner = make(label, 8, false);
        let other_method = make("t1/Loop.walk:()V", 7, false);
        let ir = make(label, 7, true);
        let listed = cache.superseded_single_pass_osr_bodies(&current);
        assert_eq!(listed.len(), 1, "only the superseded sibling");
        assert!(Arc::ptr_eq(&listed[0], &old));
        assert!(cache.superseded_single_pass_osr_bodies(&ir).is_empty(), "IR current");
        if cache.osr_profit_exit_enabled() {
            assert_eq!(cache.arm_osr_profit_exit(&old), Ok(1));
            assert!(
                cache.superseded_single_pass_osr_bodies(&current).is_empty(),
                "armed already"
            );
        }
        drop(listed);
        for b in [current, old, other_owner, other_method, ir] {
            crate::defer_jit_owner(Some(b));
        }
    }
}

/// JIT r25 (`CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE`): which retired
/// baked callees an unpublished body may still publish over
/// (`JitCache::superseded_callee_still_callable`).
#[cfg(all(test, target_arch = "x86_64"))]
mod r25_publish_over_superseded_callee_tests {
    use super::*;
    use crate::ExecutableBuffer;
    use std::sync::atomic::Ordering;

    const KEY: &str = "CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE";

    /// `NOP5; MOV EAX, value; RET`, carrying `recorded` as its inlined methods.
    fn body(value: u8, recorded: &[(String, String, String)]) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]);
        let mut cm = CompiledMethod::new(buf);
        cm.inlined_methods = recorded.to_vec();
        cm
    }

    /// The not-entrant record of `r25/S.m()I`.
    fn record() -> Box<NotEntrantRecord> {
        Box::new(NotEntrantRecord::new(
            &Arc::from("r25/S"),
            &Arc::from("m"),
            &Arc::from("()I"),
            0x2501,
            true,
            0,
            false,
            0,
        ))
    }

    /// Never reached: no test calls a patched entry.
    extern "C" fn unreached(_record: i64, _regs: i64, _stack_args: i64) -> i64 {
        0
    }

    fn caller() -> crate::JitKey {
        crate::JitKey {
            class_name: Arc::from("r25/Caller"),
            method_name: Arc::from("compile"),
            descriptor: Arc::from("()V"),
            declaring_class_id: cratonvm_types::ClassId::new(0x2502),
        }
    }

    #[test]
    fn the_pin_switch_is_default_on_with_a_zero_kill() {
        // R25-FLIP: unset and `=1` read on, `=0` off.
        const PIN: &str = "CRATONVM_JIT_PIN_BAKED_CALLEES_WHILE_COMPILING";
        cratonvm_types::flags::with_thread_overrides(&[(PIN, None)], || {
            assert!(crate::pin_baked_callees_while_compiling_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(PIN, Some("0"))], || {
            assert!(!crate::pin_baked_callees_while_compiling_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(PIN, Some("1"))], || {
            assert!(crate::pin_baked_callees_while_compiling_enabled())
        });
    }

    #[test]
    fn the_switch_is_default_on_with_a_zero_kill() {
        // R25-FLIP: unset and `=1` read on, `=0` off.
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(crate::publish_over_superseded_callee_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!crate::publish_over_superseded_callee_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(crate::publish_over_superseded_callee_enabled())
        });
    }

    /// Only the supersede redirect's shape is admitted: superseded AND not
    /// entrant, under the switch, until a logged invalidation withdraws it.
    #[test]
    fn only_a_redirected_superseded_callee_stays_callable() {
        let cache = JitCache::new();
        let caller = caller();
        let callee = body(
            7,
            &[("r25/Dep".to_string(), "m".to_string(), "()V".to_string())],
        );
        callee.stamp_published_key(&Arc::from("r25/S"), &Arc::from("m"), &Arc::from("()I"), 0x2501);
        let ask = |on: bool, c: &CompiledMethod| {
            cratonvm_types::flags::with_thread_overrides(
                &[(KEY, Some(if on { "1" } else { "0" }))],
                || cache.superseded_callee_still_callable(c, &caller),
            )
        };
        assert!(!ask(true, &callee), "a live body is not the redirect's");
        callee.superseded.store(true, Ordering::SeqCst);
        assert!(
            !ask(true, &callee),
            "an entrant body: the redirect refused or never ran"
        );
        callee
            .install_not_entrant_stub(record(), unreached as usize)
            .expect("patch");
        callee.retired.store(true, Ordering::SeqCst);
        assert!(ask(true, &callee), "the redirect's shape: superseded, forwarding");
        assert!(
            !ask(false, &callee),
            "off: every retired callee refuses, as before"
        );
        assert_eq!(cache.invalidate_for_class_change("r25/Unrelated"), 0);
        assert!(ask(true, &callee), "an unrelated define leaves it callable");
        // A class the callee inlined from gains a subclass: the log withdraws it.
        assert_eq!(cache.invalidate_for_class_change("r25/Dep"), 0);
        assert!(
            !ask(true, &callee),
            "an invalidation that withdraws the callee refuses"
        );
    }

    /// A callee never published names no key: refused.
    #[test]
    fn a_callee_without_a_key_is_refused() {
        let cache = JitCache::new();
        let callee = body(7, &[]);
        callee.superseded.store(true, Ordering::SeqCst);
        callee
            .install_not_entrant_stub(record(), unreached as usize)
            .expect("patch");
        let caller = caller();
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(!cache.superseded_callee_still_callable(&callee, &caller))
        });
    }
}

/// JIT r25 wave 6 (review lane W6-R): `JitCache::clear_all` empties the
/// invalidation log without raising `dropped_through`, and a superseded callee
/// is in no map, so nothing the flush does marks it. Before the fix the empty
/// log answered `superseded_callee_still_callable` with `true` for a callee
/// compiled BEFORE the flush; a callee from before a scoped redefinition of
/// its class is held to the same rule. Each refusal has a positive control: a
/// callee of the same shape built after the event is admitted.
#[cfg(all(test, target_arch = "x86_64"))]
mod r25_w6_superseded_callee_generation_tests {
    use super::*;
    use crate::ExecutableBuffer;
    use std::sync::atomic::Ordering;

    const KEY: &str = "CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE";
    const CLASS_ID: u32 = 0x25_0601;

    /// Never reached: no test calls a patched entry.
    extern "C" fn unreached(_record: i64, _regs: i64, _stack_args: i64) -> i64 {
        0
    }

    /// A published-then-superseded body of `r25w6/S.m()I` in the supersede
    /// redirect's shape (superseded, not entrant, retired), built now.
    fn redirected_callee() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, 7, 0x00, 0x00, 0x00, 0xC3]);
        let cm = CompiledMethod::new(buf);
        cm.stamp_published_key(
            &Arc::from("r25w6/S"),
            &Arc::from("m"),
            &Arc::from("()I"),
            CLASS_ID,
        );
        cm.superseded.store(true, Ordering::SeqCst);
        let record = Box::new(NotEntrantRecord::new(
            &Arc::from("r25w6/S"),
            &Arc::from("m"),
            &Arc::from("()I"),
            CLASS_ID,
            true,
            0,
            false,
            0,
        ));
        cm.install_not_entrant_stub(record, unreached as usize)
            .expect("patch");
        cm.retired.store(true, Ordering::SeqCst);
        cm
    }

    fn caller() -> crate::JitKey {
        crate::JitKey {
            class_name: Arc::from("r25w6/Caller"),
            method_name: Arc::from("run"),
            descriptor: Arc::from("()V"),
            declaring_class_id: cratonvm_types::ClassId::new(0x25_0602),
        }
    }

    /// The default (unset) arm: the switch is ON since R25-FLIP.
    fn ask(cache: &JitCache, callee: &CompiledMethod) -> bool {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            cache.superseded_callee_still_callable(callee, &caller())
        })
    }

    #[test]
    fn a_callee_from_before_a_full_flush_is_refused() {
        let cache = JitCache::new();
        let old = redirected_callee();
        assert!(ask(&cache, &old), "control: admitted before the flush");
        let _ = cache.clear_all();
        assert!(
            !ask(&cache, &old),
            "the flush emptied the log: a callee older than it is outside the retained window"
        );
        let fresh = redirected_callee();
        assert!(
            ask(&cache, &fresh),
            "control: a callee compiled after the flush is admitted"
        );
    }

    #[test]
    fn a_callee_from_before_a_scoped_redefinition_of_its_class_is_refused() {
        let cache = JitCache::new();
        let old = redirected_callee();
        assert!(
            ask(&cache, &old),
            "control: admitted before the redefinition"
        );
        let _ =
            cache.invalidate_for_redefinition(cratonvm_types::ClassId::new(CLASS_ID), "r25w6/S");
        assert!(!ask(&cache, &old), "compiled from the replaced bytecode");
        let fresh = redirected_callee();
        assert!(
            ask(&cache, &fresh),
            "control: a callee compiled after it is admitted"
        );
    }
}

// JIT r25 wave 10, lane W10-2b: a refused supersede forward is named.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_w10_2b_forward_refusal_tests {
    use crate::{CompiledMethod, ExecutableBuffer, ENTRY_PATCH_PAD};

    /// A body answering `value`, taking the VM context when `context`.
    fn body(value: u8, context: bool, args: u16) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, value ; RET
        let mut cm = if context {
            CompiledMethod::new_with_context(buf)
        } else {
            CompiledMethod::new(buf)
        };
        cm.entry_java_args = args;
        cm
    }

    /// The refusal is the admissibility test's own, named: the context ABI in
    /// both directions, the arity, and `None` exactly when admissible.
    #[test]
    fn w10_2b_the_refused_forward_names_its_conjunct() {
        let c1_ctx = body(7, true, 3);
        let c2_plain = body(9, false, 3);
        let c2_ctx = body(9, true, 3);
        let why = super::supersede_forward_refusal(&c1_ctx, &c2_plain).expect("refused");
        assert!(why.contains("the C1 body takes the VM context"), "{why}");
        let why = super::supersede_forward_refusal(&c2_plain, &c1_ctx).expect("refused");
        assert!(why.contains("the C2 body takes the VM context"), "{why}");
        assert_eq!(super::supersede_forward_refusal(&c1_ctx, &c2_ctx), None);
        assert!(super::supersede_forward_admissible(&c1_ctx, &c2_ctx));
        let other_arity = body(9, true, 2);
        assert_eq!(
            super::supersede_forward_refusal(&c1_ctx, &other_arity),
            Some("argument count")
        );
        for (old, new) in [
            (&c1_ctx, &c2_plain),
            (&c1_ctx, &c2_ctx),
            (&c1_ctx, &other_arity),
        ] {
            assert_eq!(
                super::supersede_forward_admissible(old, new),
                super::supersede_forward_refusal(old, new).is_none()
            );
        }
    }
}
