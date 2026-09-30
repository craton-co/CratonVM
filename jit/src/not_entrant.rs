// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Not-entrant compiled bodies (interpreter round i1 wave 22, lane L6).
//!
//! `docs/known-issues/interpreter/i21-L1-proposal-make-withdrawn-bodies-not-entrant-20260925.md`,
//! stages 1-3, for x86-64.
//!
//! # The problem
//!
//! A body withdrawn from the [`JitCache`] stays mapped for as long as anything
//! baked its address, and every baked caller keeps entering it: the plain
//! direct `CALL rel32` of a method-entry body outside a loop, the instance-site
//! direct binds, the optimizing tier's direct calls
//! (`docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md`,
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
}

/// What a not-entrant stub's forwarding prefix jumps through (wave 23, lane
/// L6): the current body's entry and the addresses of its `retired` and
/// `superseded` flags, which the stub tests before the jump, at the offsets
/// the stub bakes (0, 8, 16: `repr(C)`, 8-byte words on x86-64). Owns the
/// target, so the code it names stays mapped for as long as the record (and
/// so the patched body the record belongs to) lives.
#[repr(C)]
pub struct NotEntrantForward {
    entry: usize,
    retired_flag: usize,
    superseded_flag: usize,
    target: crate::RetainedCode,
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
        }
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
        use std::sync::atomic::Ordering;
        if !NOT_ENTRANT_FORWARDING_ENABLED || self.body_entry == 0 {
            return false;
        }
        if !self.admits_forward_target(target) {
            return false;
        }
        // Cast: an address, compared and baked only.
        let entry = target.entry_ptr() as usize;
        {
            let current = self.forward.load(Ordering::Acquire);
            if current != 0 {
                // SAFETY: a published forward word names one of `forwards`'
                // boxes, which live as long as `self`.
                let cur = unsafe { &*(current as *const NotEntrantForward) };
                if cur.entry == entry && Arc::ptr_eq(cur.target.arc(), target) {
                    return true;
                }
            }
        }
        // Every refusal below is asked again on the next slow call, which is
        // every call while there is no forward: keep the walk off that path.
        if self.forwards.lock().len() >= NOT_ENTRANT_MAX_FORWARDS
            || self.refused_target.load(Ordering::Relaxed) == target.artifact_id
        {
            return false;
        }
        // Before taking this record's lock: the walk try-locks other
        // records' lists, and must never wait on one while holding its own.
        if reaches_artifact(target, self.body_artifact_id) {
            self.refused_target
                .store(target.artifact_id, Ordering::Relaxed);
            return false;
        }
        let mut kept = self.forwards.lock();
        if kept.len() >= NOT_ENTRANT_MAX_FORWARDS {
            return false;
        }
        let forward = Box::new(NotEntrantForward {
            entry,
            // Cast: the flags' addresses, baked for the stub's byte tests. An
            // `AtomicBool` has `bool`'s in-memory representation.
            retired_flag: &target.retired as *const std::sync::atomic::AtomicBool as usize,
            // Cast: as above.
            superseded_flag: &target.superseded as *const std::sync::atomic::AtomicBool as usize,
            target: crate::RetainedCode::new(Arc::clone(target)),
        });
        // Cast: the forward's address, the word the stub reads.
        let word = &*forward as *const NotEntrantForward as usize;
        kept.push(forward);
        self.forward.store(word, Ordering::Release);
        true
    }

    /// The static half of [`Self::forward_to`]'s admission.
    fn admits_forward_target(&self, target: &CompiledMethod) -> bool {
        use std::sync::atomic::Ordering;
        // Cast: an address, compared only.
        let entry = target.entry_ptr() as usize;
        entry != 0
            && entry != self.body_entry
            && target.artifact_id != self.body_artifact_id
            // Cast: an address, tested for the inline-cache context tag.
            && (entry as u64) & crate::JIT_IC_NEEDS_CONTEXT_TAG == 0
            && target.needs_context == self.needs_context
            && target.entry_java_args != u16::MAX
            && usize::from(target.entry_java_args) == self.num_java_args
            && !target.requires_wrapped_entry
            && !target.has_indy_trap
            && !target.retired.load(Ordering::SeqCst)
            && !target.superseded.load(Ordering::SeqCst)
            && !target.is_not_entrant()
            // Wave 24: a GPU kernel keeps its dispatch helper, where the
            // offload hook lives -- what the direct-call planning of both
            // tiers asks before it bakes a static call, and what a caller
            // compiled before the kernel was known could not have asked.
            && !(self.info.invoke_kind == 3
                && crate::offload_hook::keeps_dispatch_helper(
                    self.info.class_name,
                    self.info.method_name,
                    self.info.descriptor,
                ))
            && target
                .published_key
                .get()
                .is_some_and(|(class_name, method_name, descriptor, class_id)| {
                    &**class_name == self.info.class_name
                        && &**method_name == self.info.method_name
                        && &**descriptor == self.info.descriptor
                        && *class_id == self.info.declaring_class_id
                })
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
        Some(Arc::clone(cur.target.arc()))
    }
}

/// Does `from` reach the artifact `artifact_id` through baked callee roots or
/// not-entrant forwards, at any depth? `true` also when the walk cannot tell
/// (too many bodies, or another record's forward list is busy): the caller
/// refuses a forward on `true`, which only keeps the VM path.
fn reaches_artifact(from: &Arc<CompiledMethod>, artifact_id: u64) -> bool {
    let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
    let mut stack: Vec<Arc<CompiledMethod>> = vec![Arc::clone(from)];
    while let Some(body) = stack.pop() {
        if body.artifact_id == artifact_id {
            return true;
        }
        if !seen.insert(body.artifact_id) {
            continue;
        }
        if seen.len() > FORWARD_REACH_LIMIT {
            return true;
        }
        for root in &body._direct_callee_roots {
            stack.push(Arc::clone(root.arc()));
        }
        if let Some(record) = body.not_entrant.get() {
            let Some(forwards) = record.forwards.try_lock() else {
                return true;
            };
            for forward in forwards.iter() {
                stack.push(Arc::clone(forward.target.arc()));
            }
        }
    }
    false
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
        self.copied_classes.binary_search(&class_id.as_u32()).is_ok()
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
    pub(crate) fn is_osr_body(&self) -> bool {
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
    fn entry_resumable_as_obsolete(&self) -> bool {
        self.compile_cp_stamp.is_some()
            && self.compiled_source().is_some_and(|source| {
                !source.is_synchronized
                    && !crate::bytecode_holds_monitor(&source.code, source.code.len())
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
        let Some((class_name, method_name, descriptor, class_id)) = self.published_key.get()
        else {
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
        Ok(Box::new(NotEntrantRecord::new(
            class_name,
            method_name,
            descriptor,
            *class_id,
            is_static,
            hook.vm_ptr,
            self.needs_context,
            n,
        )))
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
/// `docs/known-issues/interpreter/i44-L2-proposal-an-own-class-compiled-activation-leaves-at-its-constant-pool-sites-20261008.md`).
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
/// `docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`).
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
pub(crate) fn forced_exit_poll_cc(
    code: &[u8],
    code_addr: usize,
    cc: usize,
    flag: usize,
) -> Option<u8> {
    let op = *code.get(cc)?;
    if (op != 0x84 && op != 0x85) || cc < 5 || code[cc - 1] != 0x0F || code[cc - 2] != 0xFF {
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
    /// (`docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`).
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
    /// `docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`):
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
    ) -> (usize, Vec<crate::WithdrawnMethodKey>, Vec<Arc<CompiledMethod>>) {
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
                    stack.push(Arc::clone(forward.target.arc()));
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
            if dbg && (body.compiled_via_osr || !body.ir_osr_entries.is_empty()) {
                eprintln!(
                    "[cratonvm-jitc] exit polls candidate: {} id={} osr=true ir={} \
                     withdrawn={withdrawn} own-class={own} sites={} post-call-sites={} \
                     cp-helper-sites={} forced-before={}",
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
            if !leaves && (own || obsolete) {
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
        let now = self.withdrawn_by_redefinition.load(Ordering::Acquire).max(1);
        self.unforced_withdrawal_at.fetch_max(now, Ordering::AcqRel);
    }

    /// Were the exit polls of every body withdrawn since `before` (a
    /// [`Self::bodies_withdrawn_by_redefinition`] snapshot) forced, so that a
    /// frame running one leaves at its next exit-capable back edge without
    /// any pause (interpreter round i1 wave 26, lane L6; stage 2 of
    /// `docs/internal/fixed-bugs/interpreter-L6-proposal-retire-the-loop-exit-retries-for-forced-bodies-RETIRED-20261003.md`)?
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
            // caller, with its old constant-pool indices.
            self.note_unpatched(body);
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
            // it may leave at its next back-edge poll (wave 23).
            if !body
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
                 refused={total_refused})",
                bodies.len(),
                refused[NOT_ENTRANT_REFUSED_PUBLISHED],
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
    /// `docs/internal/fixed-bugs/interpreter-L6-a-refused-not-entrant-patch-leaves-an-old-body-below-the-census-floor-FIXED-20260927.md`).
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

/// Interpreter round i1 wave 22, lane L6: the stub's ABI, the patch, and the
/// cache's redefinition pass, on hand-built bodies (the single-pass body is
/// covered in `x64/tests.rs`).
#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::ExecutableBuffer;

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
        assert_eq!(unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call"), 8204);
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
        assert_eq!(got, 1003, "a static method is re-dispatched as invokestatic");
        assert_eq!(cache.not_entrant_counts().0, 1);
        // Wave 23: withdrawn by the redefinition, for the back-edge poll.
        assert!(stale[0].is_withdrawn_by_redefinition());
        assert_eq!(cache.bodies_withdrawn_by_redefinition(), 1);
        for b in stale {
            crate::defer_jit_owner(Some(b));
        }
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
        assert!(nested.copied_bytecode_of(copied), "a nested compile shares the list");
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
        let put = |class: &str, name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(Arc::from(class), Arc::from(name), Arc::from("()I"), id, body);
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
        assert_eq!(&combined[22..27], &[0x66, 0x41, 0x83, 0x3A, 0x00], "CMP WORD [R10], 0");
        assert_eq!(&combined[29..32], &[0x41, 0xFF, 0x23], "JMP [R11] closes it");
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
        assert_eq!(&forwarding[38..41], &[0x41, 0xFF, 0x23], "JMP [R11] closes it");
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
        assert!(!rec.forward_to(&current), "a superseded body is not admitted");
        current.superseded.store(false, Ordering::SeqCst);
        // SAFETY: back: the same forward serves again.
        assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 9);
        drop(old);
        crate::defer_jit_owner(Some(current));
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
        assert!(!before_flush.is_withdrawn_by_redefinition(), "never scanned");
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
        let patches = [(p, 100), (2 * p, p + 1), (4 * p, 8), (6 * p, 8), (7 * p + 8, 8)];
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
                vec![(0, 3, p, 5 * p), (3, 4, 6 * p, 7 * p), (4, 5, 7 * p + 8, 8 * p)]
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
        splicing.inlined_methods =
            vec![("i26l6/Spliced".into(), "value".into(), "()I".into())];
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
        assert_eq!(current.code_bytes()[20], 0x85, "the current body's poll is untouched");
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
        let forced =
            cache.force_withdrawn_exit_polls_after(&renumbering, own, "i45l2/Own", true);
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
        assert_eq!(entry_body.code_bytes()[20], 0x85, "a method-entry body stays spared");
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
}
