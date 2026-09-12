// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The compilation entry points.
//!
//! `compile_with_param_slots` is the production entry: it takes the parameter
//! layout, the per-bci metadata tables the interpreter resolved, and the runtime
//! helper table, drives `Compiler` through the bytecode walk, and publishes the
//! `CompiledMethod` together with its OSR entry table, oop maps and deopt
//! points. `compile` is the legacy wrapper that assumes `arg index == JVM slot`
//! — correct only for all-category-1 parameter lists, which is why it is a test
//! and AOT entry rather than the one `try_compile` uses.
//!
//! The thread-locals here are the staging channel for metadata that would
//! otherwise have to thread through an already-enormous argument list. Each is
//! *taken* (cleared) at the entry that consumes it, so a compile that bails out
//! cannot leak its staging into the next method compiled on the same worker.

use super::*;

// ---------------------------------------------------------------------------
// Public compilation entry point
// ---------------------------------------------------------------------------

thread_local! {
    /// Compact reference-field layout: per-pc `(byte_offset, is_ref)` for the
    /// next `compile()` call, set by the interpreter's execute / OSR compile
    /// paths (which use the `compile` wrapper, not `compile_with_param_slots`
    /// directly) so their getfield/putfield get inline compact codegen. Taken
    /// (cleared) by the wrapper. Empty for every other caller (tests, AOT) →
    /// legacy/helper field path. Same-thread, synchronous compile, no nesting.
    static PENDING_COMPACT_FIELD_INFO: std::cell::RefCell<Vec<(usize, u32, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static PENDING_VERIFIED_MAX_STACK: std::cell::RefCell<Option<usize>> =
        const { std::cell::RefCell::new(None) };
    /// `(start_pc, end_pc, handler_pc)` of this method's exception table,
    /// staged for the next `compile_with_param_slots` on this thread and
    /// consumed (taken) at its entry, so a compile that bails out cannot leak
    /// them into the next method compiled on this worker. Empty for every
    /// caller that does not stage them (tests, AOT, the legacy `compile`
    /// wrapper, OSR artifacts) and for every handler-free method — byte
    /// identical codegen there. See `find_bypassable_loop_headers`.
    static PENDING_EXCEPTION_RANGES: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `(start_pc, end_pc, handler_pc, catch type name)` of this method's
    /// exception table, staged for the next `compile_with_param_slots` on this
    /// thread and consumed at its entry.
    ///
    /// The same table as `PENDING_EXCEPTION_RANGES` plus the one thing a
    /// compiled `catch` needs and a liveness analysis does not: WHICH
    /// throwables each entry takes. An EMPTY name is `catch_type == 0`, the
    /// catch-all. Empty vector ⇒ no local handlers, byte-identical codegen.
    static PENDING_LOCAL_HANDLER_TABLE: std::cell::RefCell<
        Vec<(usize, usize, usize, &'static str)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
    /// The compiling method's declaring class id, staged beside the table
    /// above: a catch-type NAME is not a class identity, and this is the loader
    /// context it resolves through at runtime.
    static PENDING_LOCAL_HANDLER_CLASS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Stage the compact-field-info for the next [`compile`] call on this thread.
/// Call immediately before `compile`; the wrapper takes (clears) it.
pub fn set_pending_compact_field_info(info: Vec<(usize, u32, bool)>) {
    PENDING_COMPACT_FIELD_INFO.with(|c| *c.borrow_mut() = info);
}

/// Stage the reader/verifier max_stack for the next x64 compile on this thread.
/// Synthetic callers that do not stage it keep using the local estimator.
pub(crate) fn set_pending_verified_max_stack(max_stack: usize) {
    PENDING_VERIFIED_MAX_STACK.with(|c| *c.borrow_mut() = Some(max_stack));
}

/// Stage this method's exception table as `(start_pc, end_pc, handler_pc)` for
/// the next x64 compile on this thread. Call immediately before
/// `compile_with_param_slots`; it takes (clears) them. Consumed only by
/// `find_bypassable_loop_headers`, to treat a handler that can be entered from
/// outside a loop as an external entry into that loop's header.
///
/// `pub` (not `pub(crate)`) since 2026-08-17: the interpreter's
/// `compile_osr_artifact` stages it too, as one of the three requests the
/// RBC.6b lift needs. See that door's comment.
pub fn set_pending_exception_ranges(ranges: Vec<(usize, usize, usize)>) {
    PENDING_EXCEPTION_RANGES.with(|c| *c.borrow_mut() = ranges);
}

/// Stage this method's exception table WITH catch types, so the backend can
/// emit its own `catch` blocks and enter them from compiled code.
///
/// `(start_pc, end_pc, handler_pc, catch type name)`; an empty name is a
/// catch-all (`catch_type == 0`). The names must outlive the compiled code —
/// callers pass process-wide interned strings, the same ones `checkcast` sites
/// use. One-shot, taken at backend entry like every other staged request, so a
/// front-end bail cannot leak one method's handlers into the next compile on
/// this worker thread. Staging nothing is the pre-feature behaviour.
pub fn set_pending_local_handler_table(
    table: Vec<(usize, usize, usize, &'static str)>,
    declaring_class_id: u32,
) {
    PENDING_LOCAL_HANDLER_TABLE.with(|c| *c.borrow_mut() = table);
    PENDING_LOCAL_HANDLER_CLASS.with(|c| c.set(declaring_class_id));
}

/// RAII scope for one compile's PC -> inline-chain recording session.
///
/// # Why a guard and not a paired call
///
/// `compile_with_param_slots` has on the order of forty `return None` bail
/// paths — an unsupported opcode, a refused scan, a code buffer that overran,
/// a lowering that ran past its budget — and a session left open on one of
/// them is not merely a leak. `x64::inlining`'s rows are THREAD-LOCAL and
/// keyed by a native code OFFSET, so the next compile scheduled on this
/// worker thread would inherit rows naming offsets in a buffer that no longer
/// exists and publish them on ITS artifact. A stack walk would then expand a
/// frame into callees belonging to a method that was never compiled.
///
/// That is the same ABA hazard A18 refused a process-global registry keyed by
/// the executable buffer's base address over (`.agent-requests/A18-jit-lib.txt`),
/// arriving through a different door: an abandoned compile rather than a freed
/// and remapped buffer. Auditing forty returns by hand and keeping them
/// audited is exactly the discipline this codebase has repeatedly failed at —
/// `compile_gate.rs` exists because a fourth compile door was added without
/// one — so the close is delegated to `Drop`, which runs on every one of them
/// including a panic unwind, and no bail path has to know the session exists.
///
/// # Why the success path may close it directly
///
/// `finish_inline_frame_recording` is IDEMPOTENT: it `replace(false)`s the
/// recording flag and `mem::take`s the rows, so a second call sees no session,
/// touches nothing and returns an empty map. The success path therefore
/// assigns `cm.inline_frame_map` from a direct call and simply lets this guard
/// drop afterwards; there is no arming/disarming bool to get wrong, and no
/// state a double close could corrupt.
///
/// # Not reentrant, and does not need to be
///
/// The staging thread-locals at the top of this file already document
/// "same-thread, synchronous compile, no nesting", and no path out of codegen
/// re-enters `compile_with_param_slots` — the splice emitter walks the
/// callee's bytecode inside THIS compile rather than starting another one. A
/// nested compile would silently close the outer session; if one is ever
/// added, this guard is where it has to be handled.
struct InlineFrameSession;

impl InlineFrameSession {
    /// Discard whatever an abandoned compile left on this thread and open a
    /// fresh session. One thread-local write, plus a cached flag read; with
    /// `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` the session opens closed and every
    /// hook in `x64::inlining` bails on its first read.
    fn open() -> Self {
        crate::x64::begin_inline_frame_recording();
        // The NPE trap table rides the same session: it is described from the
        // same splice-scope stack, and a table left over from an abandoned
        // compile names sites in a DIFFERENT code buffer.
        crate::x64::begin_npe_trap_recording();
        InlineFrameSession
    }
}

impl Drop for InlineFrameSession {
    fn drop(&mut self) {
        // A discarding close. `code_len = 0` truncates every row, which is the
        // right answer for a bail: there is no artifact, so no row describes
        // live machine code. On the success path this is the second call and
        // does nothing.
        let _ = crate::x64::finish_inline_frame_recording(0);
        let _ = crate::x64::finish_npe_trap_recording();
    }
}

/// Compile a JVM bytecode method to x86-64 machine code.
///
/// When `needs_heap` is true, the compiled code expects a heap pointer as the
/// hidden first C argument, and Java parameters follow. This enables
/// JIT-compiled array allocation and element access via helper call-outs.
///
/// Legacy entry point: assumes `arg index == JVM slot`, which is correct only
/// for methods whose parameters are all category-1 (no long/double). Test call
/// sites use this; the production path (`jit/src/lib.rs::try_compile`) calls
/// [`compile_with_param_slots`] with the real parameter layout so long/double
/// parameters land in the slots their body reads.
///
/// Returns `Some(CompiledMethod)` on success, `None` if compilation fails.
#[allow(clippy::too_many_arguments)]
pub fn compile(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    multianewarray_info: Vec<(usize, i64)>,
    field_info: Vec<(usize, usize, u8)>,
    typecheck_info: Vec<(usize, *const u8, usize)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    anewarray_info: Vec<(usize, u32)>,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    direct_calls: Vec<(usize, crate::JitDirectCall)>,
    mic_slots: Vec<(usize, *const crate::JitMICSlot)>,
    pic_slots: Vec<(usize, *const crate::JitPICSlot)>,
    ldc_info: Vec<(usize, i64)>,
    ldc2w_info: Vec<(usize, i64)>,
    branch_hints: HashMap<usize, bool>,
    loop_unroll_hints: HashMap<usize, usize>,
    helpers: &JitRuntimeHelpers,
    non_escaping_new: std::collections::HashSet<usize>,
    inline_sites: HashMap<usize, crate::InlineSite>,
    string_layout: Option<crate::StringFieldLayout>,
) -> Option<CompiledMethod> {
    // This wrapper does NOT take an admission token, and that is deliberate.
    //
    // It is the legacy test entry point — its "arg index == JVM slot"
    // assumption is wrong for any method with a `long`/`double` parameter, so
    // no production path can use it, and none does (the only callers outside
    // this crate are two `#[cfg(test)]` fixtures in `vm/src/vm.rs`). Threading
    // a token through it would have meant editing ~140 unit-test call sites to
    // gate a function production cannot use.
    //
    // The escape it leaves is still visible: `for_backend_test` does not open
    // the thread scope, so anything reaching the backend this way is counted by
    // `compile_gate::ungated_backend_entries()`, which the VM asserts is zero
    // over a real run. `compile_with_param_slots` — the entry point the three
    // real doors use — is the one that requires the token.
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code_len,
        num_params,
        max_locals,
        needs_heap,
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        new_info,
        // Deferred (not-yet-loaded) `new`/`anewarray` sites: the legacy/test
        // wrapper has no constant pool to defer against, so never any.
        Vec::new(),
        anewarray_info,
        Vec::new(),
        invoke_info,
        direct_calls,
        mic_slots,
        pic_slots,
        ldc_info,
        // ldc_string_info / ldc_class_info: the legacy/test wrapper has no
        // constant pool to resolve either against, so never any.
        Vec::new(),
        Vec::new(),
        ldc2w_info,
        // ldc_fp_pcs: with no constant pool there is no tag to carry, and the
        // empty set makes `stack_kinds` keep answering `Unknown` for these
        // sites — the pre-existing behaviour for a caller that resolved nothing.
        FxHashSet::default(),
        branch_hints,
        loop_unroll_hints,
        helpers,
        non_escaping_new,
        inline_sites,
        // PGO-02: legacy/test wrapper never plans a guarded virtual inline
        // (it has no profile-driven admission path at all).
        HashMap::new(),
        string_layout,
        &[],
        0,
        0, // param_oop_mask: legacy/test path seeds no oop params (conservative)
        // Compact field info staged by the caller (interpreter execute/OSR);
        // empty for tests/AOT → legacy/helper field path.
        PENDING_COMPACT_FIELD_INFO.with(|c| std::mem::take(&mut *c.borrow_mut())),
        "",         // method_key: legacy/test wrapper disables the per-bci de-spec consult
        None,       // despec: no VM, so no per-VM de-spec registry to consult
        Vec::new(), // indy_info: legacy/test wrapper passes no invokedynamic sites
        // elidable_init_pcs: no constant pool here, so nothing is PROVEN empty
        // and nothing may be elided. See the parameter's doc.
        None,
    )
}

#[allow(clippy::too_many_arguments)]
/// Prove that every hot recursive edge in this body is GC-inert.
///
/// A raw `invokestatic` (no invoke/direct-call metadata) is the x64 backend's
/// representation of a self call; `try_compile` only leaves a site raw after
/// resolving it to the current method. The strict opcode whitelist excludes
/// allocation, arbitrary helpers, monitors, exception creation, arrays, and
/// loops. Forward branches and resolved inline `getfield` are harmless.
pub(super) fn gc_inert_selfrec_candidate(
    code: &[u8],
    code_len: usize,
    field_info: &[(usize, usize, u8)],
    new_info: &[(usize, u32, usize, bool, bool)],
    new_deferred_info: &[(usize, u32, u16)],
    anewarray_info: &[(usize, u32)],
    anewarray_deferred_info: &[(usize, u32, u16)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    direct_calls: &[(usize, crate::JitDirectCall)],
    mic_slots: &[(usize, *const crate::JitMICSlot)],
    pic_slots: &[(usize, *const crate::JitPICSlot)],
    indy_info: &[(usize, usize, u8, Vec<u8>, usize)],
) -> bool {
    if !gc_inert_selfrec_enabled()
        || !new_info.is_empty()
        // A deferred `new`/`anewarray` allocates too — the opcode whitelist
        // below already excludes 0xbb/0xbd, but keep the metadata gate
        // symmetric with the resolved lists so a future whitelist change
        // cannot silently admit an allocating body here.
        || !new_deferred_info.is_empty()
        || !anewarray_info.is_empty()
        || !anewarray_deferred_info.is_empty()
        || !invoke_info.is_empty()
        || !direct_calls.is_empty()
        || !mic_slots.is_empty()
        || !pic_slots.is_empty()
        || !indy_info.is_empty()
    {
        return false;
    }

    let mut pc = 0usize;
    let mut self_calls = 0usize;
    let mut saw_return = false;
    while pc < code_len {
        let op = code[pc];
        let allowed = match op {
            0x00..=0x11 | 0x15..=0x2d | 0x36..=0x4e | 0x57..=0x6b | 0x74..=0x98 => true,
            // Conditional branches and forward goto only. A backward edge
            // would need cooperative polling and is therefore not GC-inert.
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                if pc + 2 >= code_len {
                    return false;
                }
                let rel = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
                rel > 0 && pc.checked_add_signed(rel).is_some_and(|t| t < code_len)
            }
            0xac..=0xb1 => {
                saw_return = true;
                true
            }
            0xb4 => field_info.iter().any(|(field_pc, _, _)| *field_pc == pc),
            0xb8 => {
                self_calls += 1;
                true
            }
            _ => false,
        };
        if !allowed {
            return false;
        }
        let len = bytecode_len_at(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return false;
        }
        pc += len;
    }
    pc == code_len && self_calls != 0 && saw_return
}

/// Compile a method to native code with an explicit parameter→JVM-slot map.
///
/// `param_jvm_slots[i]` is the JVM local slot of the i-th incoming JIT
/// argument (`this` first for instance methods, then declared params), and
/// `param_slot_span` is the total JVM slots the parameters occupy (category-2
/// counted as 2). These let the prologue place long/double parameters in the
/// slots the body actually reads. Pass `&[]` / `0` for the legacy
/// "arg index == slot" behavior (see the [`compile`] wrapper).
/// Total callee bytecode one splice emits, INCLUDING every body it splices in
/// turn.
///
/// [`crate::InlineSite::nested_sites`] holds full recursive `InlineSite`s, and
/// the emitter splices those bodies into the same code buffer as the body that
/// contains them. Anything that sizes a reservation from a site must therefore
/// walk the whole tree, not just its root — see the two call sites in
/// [`compile_with_param_slots`] for what under-counting cost.
pub(super) fn spliced_bytecode_len(site: &crate::InlineSite) -> usize {
    site.nested_sites
        .iter()
        .map(|n| spliced_bytecode_len(&n.site))
        .fold(site.callee_code_len, |a, b| a.saturating_add(b))
}

/// Spill slots one splice needs, INCLUDING every nested body.
///
/// Same tree walk as [`spliced_bytecode_len`], against the per-site formula the
/// enclosing reservation has always used: `max(callee_max_locals, param_span)`
/// for the body's own frame, plus `callee_code_len` to bound its operand depth.
/// A nested body gets its own locals and its own operand stack on top of the
/// body that splices it, so the reserves add.
/// DEFAULT ON. Opt out with `CRATONVM_JIT_NO_INLINE_RESERVE_PATH=1`, which
/// puts the inline spill reserve back on a SUM over every site.
///
/// The arm exists because this changes the frame layout of every method that
/// inlines anything, and the previous change to a frame layout in this
/// subsystem -- reserving the scratch home at push time, 2026-09-02 -- shipped
/// a nondeterministic heap corruption that cost a rebuild per hypothesis to
/// bisect because no flag could separate it in one binary.
pub(super) fn inline_reserve_path_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_RESERVE_PATH")
    })
}

/// What ONE splice takes: its callee's locals, its branch-merge area, and a
/// bound on its own operand depth.
///
/// `callee_code_len` is the operand bound — every push costs at least one
/// bytecode byte — and `MAX_INLINE_MERGE_DEPTH` is the merge area
/// `try_emit_inline` reserves whether or not the body branches.
///
/// The merge area was missing here until 2026-09-02, which under-budgeted every
/// site by four words: `iconst_0; ireturn` is a two-byte callee that budgets
/// two words against a spend of six. It did not bite because the reserve was a
/// SUM over every site, so slack from the others covered it. Making the reserve
/// a max over concurrently-live splices removes that mask, so the four words
/// have to be named — and they are cheap next to the 273 the max saves.
fn spliced_site_own(site: &crate::InlineSite, param_span: usize) -> usize {
    site.callee_max_locals
        .max(param_span)
        .saturating_add(site.callee_code_len)
        .saturating_add(super::inlining::MAX_INLINE_MERGE_DEPTH)
}

pub(super) fn spliced_stack_reserve(site: &crate::InlineSite) -> usize {
    let (_, param_span) = crate::compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
    site.nested_sites
        .iter()
        .map(|n| spliced_stack_reserve(&n.site))
        .fold(spliced_site_own(site, param_span), |a, b| {
            a.saturating_add(b)
        })
}

/// What one site would need if concurrently-live splices were counted rather
/// than all of them: its own frame plus the DEEPEST nested path under it,
/// instead of the sum over every descendant.
///
/// Measurement only for now. A splice's epilogue rewinds `next_spill_offset`
/// to `caller_post_pop_spill` on both the value-returning and the void return
/// arm, and the outer walk runs `reset_spills()` at every instruction boundary
/// on top of that -- so sibling splices demonstrably reuse the same words, and
/// only a root-to-leaf chain is ever live at once. `spliced_stack_reserve` sums
/// siblings anyway, which is what this exists to price.
pub(super) fn spliced_stack_reserve_path(site: &crate::InlineSite) -> usize {
    let (_, param_span) = crate::compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
    let own = spliced_site_own(site, param_span);
    let deepest = site
        .nested_sites
        .iter()
        .map(|n| spliced_stack_reserve_path(&n.site))
        .max()
        .unwrap_or(0);
    own.saturating_add(deepest)
}

#[allow(clippy::too_many_arguments)]
pub fn compile_with_param_slots(
    // ── The admission gate, enforced by the type system ───────────────
    //
    // Proof that the caller passed `compile_gate::admit` — the kill switch,
    // the permanent bail-list, the bisect levers, the code-cache cap, and the
    // compile-epoch witness opened BEFORE any constant-pool read. There are
    // three doors into this function and for a long time only one of them
    // asked all of that; the other two carried hand-copied subsets, each added
    // after its own bug. `osr-01`'s brief asked for the paths to be unable to
    // "drift again", and this parameter is what makes a fourth door written
    // without the gate a *compile error* rather than a red test.
    //
    // The `jit` crate's own tests are not doors — they hand this function
    // hand-built bytecode with no method identity to admit — and they use
    // `CompileAdmission::for_backend_test()`, which is deliberately still
    // visible to `compile_gate::ungated_backend_entries()`.
    //
    // Unused in the body on purpose: it is a capability, not data.
    admission: &crate::compile_gate::CompileAdmission,
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    multianewarray_info: Vec<(usize, i64)>,
    field_info: Vec<(usize, usize, u8)>,
    typecheck_info: Vec<(usize, *const u8, usize)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    // CRIT-2 — see `new_info` field doc on the compiler struct.
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    // Cold-`new` fix — see `new_deferred_info` on the compiler struct. Sites
    // whose target class was not loaded at compile time; served by the
    // CP-indexed `new_object_cp` helper. Disjoint from `new_info`.
    new_deferred_info: Vec<(usize, u32, u16)>,
    anewarray_info: Vec<(usize, u32)>,
    // `anewarray` sibling of `new_deferred_info`.
    anewarray_deferred_info: Vec<(usize, u32, u16)>,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    direct_calls: Vec<(usize, crate::JitDirectCall)>,
    mic_slots: Vec<(usize, *const crate::JitMICSlot)>,
    // HIGH-7 — Inline 4-way PIC slots passed alongside MIC slots.
    //
    // Each entry is `(bytecode_pc, &JitPICSlot as *const _)`. When a
    // PIC slot is present at a given pc, the codegen in
    // `Compiler::compile_op_invokevirtual` emits the 4-way inline
    // cascade in place of the MIC probe (PIC supersedes MIC — it is
    // a 4-entry superset). The slot itself is allocated and owned by
    // the caller (`jit/src/lib.rs::try_compile`); it must outlive the
    // compiled method, which is ensured by attaching the boxed slot
    // to `CompiledMethod._jit_pic_slots`.
    //
    // Callers that don't yet allocate PIC slots (e.g. legacy test
    // call sites that build short bytecode snippets) pass
    // `Vec::new()` and the cascade is simply not emitted at any pc.
    pic_slots: Vec<(usize, *const crate::JitPICSlot)>,
    ldc_info: Vec<(usize, i64)>,
    ldc_string_info: Vec<(usize, u32, u16)>,
    // Class-`ldc` sites — see `ldc_class_info` on the compiler struct. Served
    // by the CP-indexed `ldc_class_cp` helper; disjoint from `ldc_info` and
    // `ldc_string_info`.
    ldc_class_info: Vec<(usize, u32, u16)>,
    ldc2w_info: Vec<(usize, i64)>,
    // The floating-point half of the `ldc`-family constant-pool tags — see
    // `Compiler::ldc_fp_pcs`. Only the deopt operand-stack snapshot reads it;
    // codegen still types these constants by their consuming opcode.
    ldc_fp_pcs: FxHashSet<usize>,
    branch_hints: HashMap<usize, bool>,
    loop_unroll_hints: HashMap<usize, usize>,
    helpers: &JitRuntimeHelpers,
    non_escaping_new: std::collections::HashSet<usize>,
    inline_sites: HashMap<usize, crate::InlineSite>,
    // PGO-02: the guarded variants of a speculative virtual/interface inline
    // site — `(receiver class id, the body THAT CLASS dispatches to)`, in guard
    // order, keyed by the same pc as `inline_sites`. One entry is a Monomorphic
    // plan, two are a Bimorphic one. See
    // `docs/feature-designs/profile-guided-inlining.md`.
    //
    // Element `[0]` is ALSO the `inline_sites` entry for that pc (the primary
    // body), so the buffer/frame reservations below count it exactly once and
    // `try_emit_inline(pc)` finds it where it has always been; element `[1]`
    // exists only here and is added to those reservations explicitly.
    //
    // Deliberately NOT threaded through the loop-unroll pc-replication tuple a
    // few lines below (unlike `inline_sites` itself) — a replicated pc without
    // an entry here just falls back to normal dispatch for that unrolled copy,
    // which is always correct, only not optimized.
    inline_guard_variants: HashMap<usize, Vec<(u32, crate::InlineSite)>>,
    // Compile-time resolved `java/lang/String` field layout for the String
    // call-site intrinsics (length/charAt/hashCode/…). `None` means "String
    // layout unavailable" — String-intrinsic codegen (added by a later
    // wave) treats it as a bail-to-dispatch. See `crate::StringFieldLayout`.
    string_layout: Option<crate::StringFieldLayout>,
    param_jvm_slots: &[usize],
    param_slot_span: usize,
    // Stage A.4 (precise oop maps) — bitmask of JVM local slots holding a
    // reference parameter on entry (bit `k` ⇒ slot `k` is an oop). Seeds the
    // "must be oop" local dataflow so oop params live at an early safepoint are
    // precisely covered. `0` on the default path → byte-identical codegen.
    param_oop_mask: u64,
    // Compact reference-field layout: per-getfield/putfield `(pc, byte_offset,
    // is_ref)` so the codegen can emit an inline compact field access (no helper
    // call, no runtime layout lookup). Empty when the flag is off → the inline
    // emitters fall back to the legacy 16-byte cell / helper path.
    compact_field_info: Vec<(usize, u32, bool)>,
    // deopt-osr Step 9 follow-up (c) — this method's
    // `"<class>.<method>:<descriptor>"` key, used to consult the per-bci de-spec
    // registry (`despec` below) and suppress a loop-header speculative-BCE
    // guard that has repeatedly deopted. `""` (the legacy/test `compile()`
    // wrapper) disables the consult; an empty registry leaves codegen
    // byte-identical.
    method_key: &str,
    // The compiling VM's per-bci de-spec registry
    // (`crate::deopt::DespecRegistry`, owned by the VM's `JitRealm`). Per VM,
    // not per process: one VM's despeculation verdicts must not strip
    // speculations from another VM's compiles. `None` (no VM in scope: the
    // legacy `compile()` wrapper and crate fixtures) consults nothing.
    despec: Option<&std::sync::Arc<crate::deopt::DespecRegistry>>,
    // Resolved `invokedynamic` (0xba) call-site info — see the `indy_info`
    // field doc on the `Compiler` struct. Empty from the legacy `compile()`
    // test wrapper (which also passes no `indy_ops` to `jit_scan` callers, so
    // this is always consistent with an invokedynamic-free method there).
    indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)>,
    // Bytecode pcs of `invokespecial` sites whose target constructor the CALLER
    // has PROVEN empty (`jit_bridge::is_elidable_construction` — a 5-byte
    // `aload_0; invokespecial Object.<init>()V; return` body), reached here from
    // `try_compile_inner`'s `cp_elidable_init_resolver`.
    //
    // This is the ONLY thing that may license eliding an `<init>`. It used to be
    // re-derived from the descriptor (`method_name == "<init>" && descriptor ==
    // "()V"`), which is a check on the SIGNATURE and says nothing about the
    // body: every no-arg constructor passed, so a receiver whose constructor
    // wrote global state was marked non-escaping, scalar-replaced, and its
    // `<init>` call dropped together with the write. `EA.java` in the bug doc
    // measures it — 1,000,000 `new` whose ctor does `++someStaticInt` left the
    // counter at 0 with the JIT on and at 1,000,000 with `--nojit`.
    //
    // `None` means the caller proved nothing and NOTHING may be elided. That is
    // the safe direction and the one the IR backend already reasons in
    // ("Calling an `<init>` runs every side effect the elision path was allowed
    // to skip"). The legacy/test `compile()` wrapper and the unroll fixture pass
    // `None`; they have no constant pool to resolve against.
    //
    // Pcs are the ORIGINAL (pre-unroll) ones. A loop-unroll copy carries shifted
    // pcs that are absent from this set, so copies simply keep their `<init>`
    // calls — an optimisation left on the table, never a miscompile.
    elidable_init_pcs: Option<std::collections::HashSet<usize>>,
) -> Option<CompiledMethod> {
    // Cost of a discarded lowering, for the code-buffer bail below. Reading a
    // monotonic clock once per compile is noise next to the compile itself.
    let compile_started = std::time::Instant::now();
    // The drift witness for `compile_gate`. Every production door must hold an
    // admission token when it gets here; this counts the entries that do not,
    // which is how a FOURTH door added later announces itself instead of
    // silently skipping the admission checks the way the OSR and eager
    // first-call doors did for months. Behaviour-named on purpose: a check that
    // scanned the source for `compile_with_param_slots(` would have died the
    // day `x64.rs` was split, as five checks in this repository did.
    //
    // Non-zero inside this crate's own tests is expected and meaningless — a
    // unit test calling the backend is not a door. The assertion that matters
    // lives in the VM.
    //
    // Kept even though `admission` is now required by the signature: the two
    // layers fail differently. The parameter stops a door written *without*
    // the gate; this counter stops a door written *with*
    // `CompileAdmission::for_backend_test()`, which the type system cannot
    // tell apart from a real one.
    crate::compile_gate::note_backend_entry();
    // H20-1. The direct-call half of the same two-layer idea, and the reason
    // `admission` is no longer a `let _ =`.
    //
    // This counts; it deliberately does NOT filter. A row dropped here would be
    // unsound: `reserve_stack_floor` below keys a raw self-call site on an
    // `invokestatic` pc with *neither* an invoke-info entry *nor* a direct-call
    // plan, and every ladder pushes its row and then `continue`s past the
    // `invoke_info` construction for that pc — so deleting a row at this point
    // leaves the pc with no metadata at all and the `0xb8` arm compiles
    // `Thread.currentThread()` as a call to the enclosing method. The refusal
    // has to happen at the bind site, where falling through still builds the
    // fallback; see `CompileAdmission::admits_direct_bind`.
    //
    // What it buys: the first per-door count of direct-call rows from a door
    // that never asked the JDK-only question. `H12-1` N2 — the OSR door's
    // HashMap binds had no counter, which is why a prediction about them was
    // unfalsifiable.
    crate::compile_gate::note_direct_binds(admission, direct_calls.len());
    // A class-`ldc` calls a helper that takes the VM context as its first
    // argument, exactly like a string-`ldc`, so it forces the context form of
    // the artifact too.
    let needs_heap = needs_heap || !ldc_string_info.is_empty() || !ldc_class_info.is_empty();
    let gc_inert_selfrec = gc_inert_selfrec_candidate(
        code,
        code_len,
        &field_info,
        &new_info,
        &new_deferred_info,
        &anewarray_info,
        &anewarray_deferred_info,
        &invoke_info,
        &direct_calls,
        &mic_slots,
        &pic_slots,
        &indy_info,
    );
    let verified_max_stack = PENDING_VERIFIED_MAX_STACK.with(|c| c.borrow_mut().take());
    // One-shot like every other staged request below: take it here so an early
    // bail cannot leak this method's handler ranges into an unrelated later
    // compile on this worker thread.
    let exception_ranges: Vec<(usize, usize, usize)> =
        PENDING_EXCEPTION_RANGES.with(|c| std::mem::take(&mut *c.borrow_mut()));
    // Same one-shot discipline. Taken unconditionally — even when the feature
    // is off — so a staged table can never survive into a later compile.
    let staged_local_handlers: Vec<(usize, usize, usize, &'static str)> =
        PENDING_LOCAL_HANDLER_TABLE.with(|c| std::mem::take(&mut *c.borrow_mut()));
    let staged_local_handler_class = PENDING_LOCAL_HANDLER_CLASS.with(|c| c.replace(0));
    // Consume the pure-kernel GPR local-homes request FIRST so an early bail
    // below can never leak it into an unrelated later compile on this thread.
    let kernel_reg_homes_requested = KERNEL_REG_HOMES_REQUEST.with(|c| c.take());
    // A handler-local request is one-shot too, so a compile bailout cannot
    // accidentally arm the next unrelated method on this worker thread.
    let precise_exception_frames = PRECISE_EXCEPTION_FRAME_REQUEST.with(|c| c.take());
    // Open the PC -> inline-chain recording session for this compile, with the
    // one-shot `take`s above and for the same reason they are here: everything
    // staged on this thread is claimed BEFORE any bail can leak it into the
    // next method compiled on this worker. `open()` additionally discards
    // anything an abandoned earlier compile left behind, so it is safe
    // unconditionally and costs one thread-local write. Nothing above this
    // point emits a byte of code, and nothing above it returns.
    //
    // The close is `InlineFrameSession`'s `Drop`, not a call at the end — see
    // that type for why the ~40 bail paths below must not each be responsible
    // for it.
    let _inline_frame_session = InlineFrameSession::open();
    if crate::rbc6_emit_dbg() {
        eprintln!(
            "[rbc6-emit] driver took precise_exception_frames={precise_exception_frames}              exception_ranges={} protected_ranges_pending={}",
            exception_ranges.len(),
            PROTECTED_RANGES_REQUEST.with(|c| {
                let v = c.take();
                let n = v.as_ref().map(|r| r.len()).unwrap_or(0);
                c.set(v);
                n
            }),
        );
    }
    // Same one-shot discipline as the flag above.
    let protected_ranges = PROTECTED_RANGES_REQUEST
        .with(|c| c.take())
        .unwrap_or_default();
    // OSR-tier request (perf/halfgap-20260717): same purity conditions below,
    // but the published artifact KEEPS its OSR entries — the trampoline's
    // register-seeded entry contract is exactly what the assignments
    // describe. See `set_kernel_reg_homes_osr_request`.
    let kernel_reg_homes_osr_requested =
        KERNEL_REG_HOMES_OSR_REQUEST.with(|c| c.take()) && kernel_reg_osr_enabled();

    // ── Bytecode loop rewriter (opt-in; see `set_bytecode_loop_rewriter_armed`)
    //
    // This is the whole interception. Below this point `code`/`code_len` are
    // the REWRITTEN method and every pc-keyed side table has been lifted into
    // its PC space, so the ~40 analyses and the emitter are unmodified: they
    // simply see a different method. Three things make that sound, and all
    // three are here rather than scattered:
    //
    //   (i)   the rewrite itself, refused for anything it cannot describe;
    //   (ii)  ONE replication of ALL pc-keyed tables, in one expression, so a
    //         table cannot be forgotten silently;
    //   (iii) the coordinate change back to interpreter-bci space, which is
    //         `Compiler::orig_bci` at the three sites that BAKE a bci into
    //         machine code plus the `osr_pc_to_native` / `osr_dead_mask`
    //         rebuild at the end of this function.
    //
    // `None` on every unarmed compile, at the cost of one thread-local
    // `Cell<bool>` load, and the whole path below is then the identity.
    let loop_xform: Option<LoopXform> = match plan_bytecode_loop_xform(
        code,
        code_len,
        &exception_ranges,
        &loop_unroll_hints,
        LoopRewriteShape {
            deopt_real: crate::deopt_real_enabled(),
            precise_exception_frames,
            has_indy: !indy_info.is_empty(),
            has_inline_sites: !inline_sites.is_empty(),
        },
    ) {
        Ok(x) => {
            crate::metrics::record_loop_xform_event("loop_xform_applied");
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
                eprintln!(
                    "[JIT_GEN] bytecode loop rewrite: kind={:?} versioned={} header={} \
                     body_len={} copies={} code_len {}->{} poll_free={}",
                    x.kind,
                    x.versioning.is_some(),
                    x.header,
                    x.body_len,
                    x.copies,
                    code_len,
                    x.code_len,
                    x.poll_free_bytes
                );
            }
            Some(x)
        }
        Err(refusal) => {
            if !matches!(refusal, LoopRewriteRefusal::NotArmed)
                && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN")
            {
                eprintln!("[JIT_GEN] bytecode loop rewrite refused: {refusal:?}");
            }
            None
        }
    };
    // Kept for the coordinate change at the end of this function; `code_len`
    // is about to become the rewritten length.
    let orig_code_len = code_len;
    // A rewritten method has a backward branch, and `gc_inert_selfrec_candidate`
    // (computed above, on the ORIGINAL bytes and tables) rejects every backward
    // branch — so this conjunction is already true. Written out anyway so the
    // one predicate computed before the rewrite cannot silently start
    // describing a method that no longer exists.
    let gc_inert_selfrec = gc_inert_selfrec && loop_xform.is_none();
    let (code, code_len): (&[u8], usize) = match &loop_xform {
        Some(x) => (&x.code[..], x.code_len),
        None => (code, code_len),
    };
    // Handler ranges in output coordinates. A range enclosing the loop is
    // WIDENED over the copies by the rewriter, which is what keeps a `try`
    // that lexically encloses the loop covering every copy.
    let exception_ranges: Vec<(usize, usize, usize)> = match &loop_xform {
        Some(x) => x.exception_ranges.clone(),
        None => exception_ranges,
    };
    // The per-bci de-spec registry (`crate::deopt::DespecRegistry`) is keyed
    // by INTERPRETER bci, but every loop header below is an output pc. Consult
    // it through the provenance map. Identity when unarmed.
    let despec_bci = |pc: usize| -> u32 {
        // Cast: bci fits u32 (checked against u32::MAX by the rewriter)
        loop_xform.as_ref().and_then(|x| x.bci_at(pc)).unwrap_or(pc) as u32
    };

    // ── Side-table replication — ATOMIC BY CONSTRUCTION ────────────────
    //
    // All 21 pc-keyed tables `compile_with_param_slots` receives, rebound in
    // ONE expression. Adding a 22nd parameter and forgetting it here is a
    // compile error at the destructuring, not a silent miscompile: a copy that
    // lost its `field_info` entry takes the helper-call fallback instead of the
    // inline access and nothing fails.
    //
    // Payloads are CLONED, so the pointer-carrying tables (`typecheck_info`,
    // `invoke_info`, `mic_slots`, `pic_slots`, `ldc_string_info`) share one
    // target across the copies. For the read-only ones (a class name, a
    // resolved-invoke descriptor, an interned string) that is trivially sound.
    // For the two MUTABLE ones (`mic_slots`, `pic_slots`) it is sound because
    // an inline cache keyed on a call site sees the same receiver distribution
    // in every copy — exactly what happens today when a non-unrolled loop runs
    // many times. It is also what makes them safe to share at all: the caller
    // (`jit/src/lib.rs::try_compile`) owns and outlives these slots, and
    // minting fresh ones per copy would need code outside this crate's file.
    let (
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        new_info,
        new_deferred_info,
        anewarray_info,
        anewarray_deferred_info,
        invoke_info,
        direct_calls,
        mic_slots,
        pic_slots,
        ldc_info,
        ldc_string_info,
        ldc_class_info,
        ldc2w_info,
        branch_hints,
        loop_unroll_hints,
        non_escaping_new,
        inline_sites,
        compact_field_info,
        indy_info,
    ) = match &loop_xform {
        None => (
            multianewarray_info,
            field_info,
            typecheck_info,
            static_field_info,
            new_info,
            new_deferred_info,
            anewarray_info,
            anewarray_deferred_info,
            invoke_info,
            direct_calls,
            mic_slots,
            pic_slots,
            ldc_info,
            ldc_string_info,
            ldc_class_info,
            ldc2w_info,
            branch_hints,
            loop_unroll_hints,
            non_escaping_new,
            inline_sites,
            compact_field_info,
            indy_info,
        ),
        Some(x) => (
            x.replicate_pc_keyed(&multianewarray_info),
            replicate_pc3(x, field_info),
            replicate_pc3(x, typecheck_info),
            replicate_pc5(x, static_field_info),
            replicate_pc5(x, new_info),
            replicate_pc3(x, new_deferred_info),
            x.replicate_pc_keyed(&anewarray_info),
            replicate_pc3(x, anewarray_deferred_info),
            x.replicate_pc_keyed(&invoke_info),
            // `JitDirectCall` is not `Clone` (it lives in `jit/src/lib.rs`,
            // which this agent does not own), so its payload is packed into a
            // tuple of `Copy` fields, replicated by the same primitive, and
            // rebuilt. Adding `#[derive(Clone)]` there would let this use
            // `replicate_pc_keyed` directly.
            {
                let packed: Vec<(usize, (usize, bool, usize, u8, u32))> = direct_calls
                    .into_iter()
                    .map(|(pc, d)| {
                        (
                            pc,
                            (
                                d.entry,
                                d.needs_context,
                                d.num_params,
                                d.return_type,
                                d.guard_class_id,
                            ),
                        )
                    })
                    .collect();
                x.replicate_pc_keyed(&packed)
                    .into_iter()
                    .map(
                        |(pc, (entry, needs_context, num_params, return_type, guard_class_id))| {
                            (
                                pc,
                                crate::JitDirectCall {
                                    entry,
                                    needs_context,
                                    num_params,
                                    return_type,
                                    guard_class_id,
                                },
                            )
                        },
                    )
                    .collect::<Vec<(usize, crate::JitDirectCall)>>()
            },
            x.replicate_pc_keyed(&mic_slots),
            x.replicate_pc_keyed(&pic_slots),
            x.replicate_pc_keyed(&ldc_info),
            replicate_pc3(x, ldc_string_info),
            replicate_pc3(x, ldc_class_info),
            x.replicate_pc_keyed(&ldc2w_info),
            x.replicate_pc_keyed(&branch_hints.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, bool>>(),
            // Keyed by BACK-EDGE pc. Under `Unroll` an original back-edge bci
            // has exactly one image (the last copy carries the only back edge),
            // so this stays single-valued.
            x.replicate_pc_keyed(&loop_unroll_hints.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, usize>>(),
            x.replicate_pc_keyed(
                &non_escaping_new
                    .into_iter()
                    .map(|pc| (pc, ()))
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|(pc, ())| pc)
            .collect::<std::collections::HashSet<usize>>(),
            // Always empty here — `InlineSitesPresent` refuses the transform —
            // but routed through the same primitive so the census has no
            // "handled elsewhere" entry.
            x.replicate_pc_keyed(&inline_sites.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, crate::InlineSite>>(),
            replicate_pc3(x, compact_field_info),
            // NOT empty any more. `InvokedynamicPresent` used to refuse the
            // transform outright; since the deopt bci translation landed, a
            // method with an `invokedynamic` is rewritten like any other and
            // every copy of a `0xba` site needs its own entry here — the site
            // lowers to an unconditional trap that records a resume snapshot,
            // and a copy without an entry would bail the whole compile
            // (`indy_info_idx` miss ⇒ `return false`).
            replicate_pc5(x, indy_info),
        ),
    };

    // Estimate buffer size. A bytecode invoke is not the old ~40-byte helper
    // call: the current lowering can emit a context bridge, exception/deopt
    // edge, and MIC/PIC dispatch machinery. Hibernate's concurrent query path
    // demonstrated that the former 96-byte invoke allowance repeatedly
    // exhausted otherwise modest 10 KiB buffers, leaving hot methods in the
    // interpreter. Keep enough headroom for those sites; the code-cache cap
    // remains the global bound on retained executable memory.
    //
    // 512 -> 1024 (2026-08-01). Widening the PIC's inter-slot branch from
    // `rel8` to `rel32` grew every inline-cache site, and 512 stopped covering
    // them. Measured on one Spring Boot suite class: TEN methods overflowed per
    // run, and in every one of them `inline_extra` was 0 and the whole shortfall
    // sat in this term. Solving each for the per-invoke cost the body actually
    // needed — `(wanted - code_len * 96 - 8192) / invokes`, which OVER-attributes
    // (the `code_len * 96` term also pays for the invoke bytecodes) — gives:
    //
    //     MapperListener.containerEvent           68 invokes   957 B/invoke
    //     AbstractBeanDefinition.<init>           83 invokes   824
    //     OnBeanCondition.getMatchingBeans        34 invokes   728
    //     ObjectCreateRule.begin                  22 invokes   674
    //     ResolvableType.getNested                 6 invokes   642
    //     ClassFileAnnotationMetadata.resolveTypeName 7 invokes 640
    //     StringUtils.collectionToDelimitedString 16 invokes   578
    //     AbstractAutowireCapableBeanFactory.populateBean 34   547
    //     DateTimeFormatterBuilder$NumberPrinterParser.format 36 531
    //     jdk.internal.classfile.impl.ClassImpl.forEach 21     515
    //
    // 1024 covers the worst of them with margin. Unlike the optimizing tier —
    // which now measures the shortfall and re-runs the lowering at that size
    // (`ir_lower::lower_inner`) — this backend cannot retry: it consumes six
    // one-shot thread-local staging requests before the buffer is allocated,
    // and re-entering it would find them gone. The estimate has to be right the
    // first time here, so it errs high.
    // PGO-02 (bimorphic): a two-guard site splices a SECOND body at the same
    // pc, and that body is not in `inline_sites`. Both this buffer estimate
    // and the spill reservation below must see it — this backend cannot retry
    // a short buffer, and an unreserved inlined body writes past the spill
    // region into the callee-saved area. Skip variant `[0]`, which IS the
    // `inline_sites` entry and is already counted.
    let extra_guard_bodies = || {
        inline_guard_variants
            .values()
            .flat_map(|variants| variants.iter().skip(1).map(|(_, s)| s))
    };
    // NESTED bodies count too. `InlineSite::nested_sites` is a recursive
    // `InlineSite`, and `emit_inline_body` splices those bodies into the SAME
    // buffer as the body that contains them — so a site's real footprint is its
    // own bytecode PLUS every body it inlines in turn, transitively. Sizing
    // from `callee_code_len` alone reserved the outer body's bytes and none of
    // the nested ones, which is exactly how a method whose callees are all tiny
    // (`VolumeOps.grad`: 273 invokes of 5-15 byte getters that each splice a
    // constructor and three field loads) overran an estimate by 8% and stopped
    // being compiled at all. The per-method inline BUDGET was already
    // nested-aware (`inline_site_expansion_cost_tiered` folds
    // `nested_expansion`); only the two sizing sites here were not.
    let inline_extra: usize = inline_sites
        .values()
        .chain(extra_guard_bodies())
        .map(|s| spliced_bytecode_len(s).saturating_mul(64))
        .sum();
    // TWO per-bytecode coefficients, not one.
    //
    // 96 is calibrated on ordinary control-flow-heavy code, and it works there
    // BECAUSE the `invoke_info.len() * 1024` term carries most of the weight:
    // every method in the 2026-08-01 table above is invoke-dense, and in all of
    // them the invoke term dominates. A method with almost no invokes gets
    // nothing from that term, so 96 becomes the WHOLE estimate — and 96 is not
    // enough for the one shape that emits the most machine code per bytecode:
    // the large table initialiser.
    //
    // The witness is `java/lang/CharacterData00.<clinit>:()V`, which overran on
    // every boot of every process (a stock `Hello` reproduces it). Its shape,
    // read off `javap -c -p`: 4096 bytes of bytecode holding 2906 instructions,
    // of which 635 `dup`, 324 `castore`, 313 `iconst_0`, 311 `iconst_1`, 309
    // `aastore`, 290 `sipush`, 207 `newarray`, 206 `iconst_2`, 124 `bipush`,
    // 104 `anewarray` — and SEVEN invokes in the entire method. Nothing but
    // constant-push and array-store, at 1.41 bytecode BYTES per instruction
    // where branchy code sits nearer 3; each of those one-byte opcodes still
    // lowers to a spill/reload pair, and each store to a null check plus a
    // bounds check. So the machine code per bytecode BYTE is far above what the
    // 96 was fitted to, and no term in the old estimate noticed.
    //
    // Derivation, from that one measurement (`capacity=408576 wanted=473627`,
    // which also pins `invoke_info.len() == 7` and `inline_extra == 0`:
    // 4096*96 + 8192 + 7*1024 is exactly 408576):
    //
    //     needed per bytecode byte = (473627 - 8192 - 7*1024) / 4096 = 111.9
    //
    // 144 (= 1.5 * 96) covers that with 29% margin. The margin is deliberately
    // modest rather than generous: `ExecutableBuffer::new` charges the WHOLE
    // capacity to `COMMITTED_JIT_CODE_BYTES`, which is the quantity the
    // code-cache cap bounds, so every byte over-estimated here is a byte the
    // cap will not spend on some other method.
    //
    // TRUST THIS EXACTLY AS FAR AS ONE MEASUREMENT GOES. 111.9 is a single
    // number from a single method. The only cross-check available without
    // running the VM is `java/lang/CharacterDataLatin1.<clinit>`, the same
    // shape — 3097 instructions in ~4718 bytes, ONE invoke. Scaling the
    // witness's 163 machine bytes per bytecode INSTRUCTION (473627/2906) predicts
    // ~107 bytes per bytecode byte for it: under 144, and also over 96, i.e. a
    // second method the old coefficient was short for. That is a PREDICTION,
    // not a measurement. If a third shape overruns at 144, re-derive from its
    // own `wanted` instead of nudging this number.
    //
    // The predicate is deliberately cheap and honest — no bytecode walk, only
    // `code_len` and the invoke list the caller already built. LARGE, because a
    // small method's shortfall is cheap and the hint below self-corrects it in
    // one retry; LOW INVOKE DENSITY, because that is precisely the condition
    // under which the 1024-per-invoke term stops covering for 96. One invoke
    // per 512 bytecode bytes puts the witness (7 invokes over 4096 bytes)
    // inside and every method in the table above outside.
    const BYTES_PER_BYTECODE: usize = 96;
    const BYTES_PER_BYTECODE_TABLE_INIT: usize = 144;
    const TABLE_INIT_MIN_CODE_LEN: usize = 2048;
    const TABLE_INIT_BYTECODES_PER_INVOKE: usize = 512;
    let table_init_shaped = code_len >= TABLE_INIT_MIN_CODE_LEN
        && invoke_info
            .len()
            .saturating_mul(TABLE_INIT_BYTECODES_PER_INVOKE)
            < code_len;
    // ENGAGEMENT, not just a number: without this there is no way to tell a run
    // where the second coefficient prevented an overflow from a run where the
    // predicate never matched anything. Cheap — the env read is behind the
    // shape test, so an ordinary method never performs it.
    if table_init_shaped && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
        eprintln!(
            "[cratonvm-jitc] code-buffer estimate: table-init shape \
             method={} code_len={} invokes={} bytes_per_bytecode={}",
            method_key,
            code_len,
            invoke_info.len(),
            BYTES_PER_BYTECODE_TABLE_INIT
        );
    }
    let estimated_size = code_len
        .saturating_mul(if table_init_shaped {
            BYTES_PER_BYTECODE_TABLE_INIT
        } else {
            BYTES_PER_BYTECODE
        })
        .saturating_add(8192)
        .saturating_add(invoke_info.len().saturating_mul(1024))
        .saturating_add(inline_extra);
    // A previous attempt at this method that emitted past its estimate left the
    // size it actually wanted behind; prefer the measurement to the heuristic.
    // See `crate::note_code_buffer_shortfall` — without this, the estimate has
    // exactly one chance per method and getting it wrong is permanent.
    let estimated_size = match crate::code_buffer_hint(method_key) {
        Some(measured) => estimated_size.max(measured),
        None => estimated_size,
    };
    let mut buf = ExecutableBuffer::new(estimated_size.max(4096))?;
    buf.set_tag("x64-single-pass");

    // Size operand-stack spills from the reader/verifier max_stack when the
    // production path supplies it. Keep the local estimator as a defensive floor
    // for legacy tests and future synthetic call sites.
    let estimated_max_stack = estimate_max_stack(code, code_len);
    let max_stack = verified_max_stack
        .map(|verified| verified.max(estimated_max_stack))
        .unwrap_or(estimated_max_stack);
    // Bug-4 frame sizing, part B: the invoke-dispatch sites carve their
    // outgoing args buffer at the CURRENT spill watermark and extend it by
    // n*8 bytes for the call's duration. At worst (operand stack at
    // max_stack depth when the deepest-arity call is emitted) the buffer
    // tops out n slots past the spill region — overlapping the callee-saved
    // save area, or, past `frame_size`, the callee's own stack (where the
    // next CALL's return-address push zeroes it). Reserve the worst-case
    // arity on top of the estimate so the buffer always stays inside the
    // reserved frame.
    let max_invoke_args: usize = invoke_info
        .iter()
        // SAFETY: invoke_info pointers are kept alive by the caller for the
        // duration of compilation (same contract as the emission sites).
        .map(|(_, p)| unsafe { (**p).num_jit_args })
        .max()
        .unwrap_or(0);
    // Inlining allocates extra spill slots for each inlined callee's locals
    // and operand stack ON TOP of the caller's `max_stack`. `spill_size` is
    // derived purely from `max_stack`, so without this reserve the inlined code
    // writes past the spill region into the callee-saved / shadow area —
    // corrupting live values (observed as a `ClassCastException: …$TaskOption
    // not an enum` when a clobbered slot fed an enum-typed field). Reserve, per
    // site, `callee_max_locals + callee_code_len` (the latter bounds the
    // callee's own operand depth).
    //
    // This used to read "...and, since the inline epilogue keeps the return
    // value rather than reclaiming the callee locals, sequential inlines
    // accumulate", and summed the per-site figures on that basis. The epilogue
    // reclaims now — see `inline_reserve_path_enabled` below for the evidence
    // and for what replaced the sum.
    let inline_stack_reserve_sum: usize = inline_sites
        .values()
        .chain(extra_guard_bodies())
        .map(spliced_stack_reserve)
        .sum();
    let inline_stack_reserve_path: usize = inline_sites
        .values()
        .chain(extra_guard_bodies())
        .map(spliced_stack_reserve_path)
        .max()
        .unwrap_or(0);
    // Spend the concurrently-live figure, not the sum. DEFAULT ON; opt out
    // with `CRATONVM_JIT_NO_INLINE_RESERVE_PATH=1`.
    //
    // The sum is what the comment above asks for, and it was right when it was
    // written: the splicer used to keep the return value and leave the callee
    // locals where they were. It does not any more. Both of the inline
    // epilogue's return arms end with
    // `self.next_spill_offset = caller_post_pop_spill`, and the outer walk
    // calls `reset_spills()` at every instruction boundary on top of that, so
    // sibling splices provably reuse the same words -- the void arm's own
    // comment says the reclaim is load-bearing precisely for the NESTED case,
    // where the mini-walk has no per-instruction reset. Only a root-to-leaf
    // chain is ever live at once, which is what `spliced_stack_reserve_path`
    // measures.
    //
    // Measured before the change: 280 words reserved against 7 needed on a
    // 40-argument stress, 265 against 41 on CratonBench -- 2 KB of frame per
    // compiled method to hold one 56-byte splice.
    //
    // Under-reserving here FAILS CLOSED. `callee_local_base`, the merge area
    // and every callee operand push all go through `reserve_spill_slots`, which
    // bounds against `spill_limit_offset` and bails the compile rather than
    // writing past the region; the `exhausted` census column counts exactly
    // that. So the worst case of this being wrong is inlining declined and a
    // method left interpreted, visible in the census -- not a clobbered frame.
    let inline_stack_reserve = if inline_reserve_path_enabled() {
        inline_stack_reserve_path
    } else {
        inline_stack_reserve_sum
    };
    crate::note_inline_reserve(
        inline_stack_reserve_sum as u64,
        inline_stack_reserve_path as u64,
        inline_stack_reserve as u64,
    );
    let max_stack = max_stack
        .saturating_add(max_invoke_args)
        .saturating_add(inline_stack_reserve);

    // LICM: detect loops and find invariant aaload sequences to hoist
    let loops = detect_loops(code, code_len);
    // Pre-header placement soundness: a loop header that can be entered by a
    // branch from outside the loop would run the loop body with an
    // uninitialised hoist slot / an unrun speculative guard, because
    // `pc_to_native[header]` deliberately points PAST the pre-header. Drop
    // every speculating transform for such headers — see
    // `find_bypassable_loop_headers` for the full derivation and the
    // `AttributesImpl.ensureCapacity` witness.
    let bypassable_headers =
        find_bypassable_loop_headers(code, code_len, &loops, &exception_ranges);
    let hoist_info = if cratonvm_types::flags::runtime_flag_on("CRATONVM_DISABLE_AALOAD_LICM") {
        Vec::new()
    } else {
        find_loop_hoists(code, code_len, &loops)
    };
    // Per-bci de-spec (same registry the speculative-BCE guards use): the
    // hoisted aaload's null+bounds preheader guard deopts at the loop-header
    // bci; once a header crosses the de-spec threshold, drop its hoists so
    // the recompile emits the in-loop aaload with its normal checks instead
    // of re-making the failed speculation. Must run BEFORE `Compiler::new`
    // pairs `hoist_offsets` with `hoist_info` by index.
    let hoist_info: Vec<LoopHoist> = hoist_info
        .into_iter()
        .filter(|h| {
            let despec = despec
                .is_some_and(|registry| registry.contains(method_key, despec_bci(h.loop_header)));
            if despec && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                eprintln!(
                    "[cratonvm-deopt] de-spec: dropping aaload LICM hoist at loop_header \
                     bci={} for {} (recompile with in-loop checked access)",
                    h.loop_header, method_key
                );
            }
            !despec
        })
        .collect();
    let hoist_info: Vec<LoopHoist> = hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();

    // LICM: hoist the loop-invariant `arraylength` out of a counted loop's
    // header. `CRATONVM_DISABLE_ARRAYLEN_LICM=1` is the kill switch — the
    // hoist changes the emitted body of essentially every loop over an array
    // in the VM, so it needs one, and the bisect it serves must reach the
    // level the change is at (the emission, not the analysis).
    let array_len_hoist_info =
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DISABLE_ARRAYLEN_LICM") {
            Vec::new()
        } else {
            find_array_len_hoists(code, code_len, &loops)
        };
    // One filter, not the aaload hoist's two. There is no per-bci de-spec to
    // apply because this pre-header speculates on nothing: it throws the NPE
    // the body would have thrown rather than deopting, so there is no failed
    // guard for a de-spec threshold to count.
    //
    // The bypassable-header veto DOES apply, and is the load-bearing one. A
    // header reachable without running its own pre-header — a `goto` from
    // outside into the loop, or an exception handler landing in the body —
    // leaves the slot cold, and a cold slot here is a garbage LENGTH that a
    // `bounds_safe_pcs` access then trusts, i.e. an unchecked out-of-bounds
    // read rather than a wrong answer. Must run BEFORE `Compiler::new` pairs
    // the offsets with the info by index.
    let array_len_hoist_info: Vec<ArrayLenHoist> = array_len_hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();
    if !array_len_hoist_info.is_empty()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN")
    {
        eprintln!(
            "[JIT_GEN] arraylength-LICM hoists={} sites={:?}",
            array_len_hoist_info.len(),
            array_len_hoist_info
                .iter()
                .map(|h| (h.loop_header, h.array_local, h.sites.len()))
                .collect::<Vec<_>>(),
        );
    }

    // LICM: find loop-invariant integer-arithmetic runs to hoist into the
    // loop pre-header. These are pure, non-faulting ALU expressions on
    // loop-invariant locals/constants — see `find_arith_loop_hoists`.
    let arith_hoist_info =
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DISABLE_ARITH_LICM") {
            Vec::new()
        } else {
            find_arith_loop_hoists(code, code_len, &loops)
        };
    let arith_hoist_info: Vec<ArithLoopHoist> = arith_hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();
    if !arith_hoist_info.is_empty()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN")
    {
        eprintln!(
            "[JIT_GEN] arith-LICM hoists={} runs={:?}",
            arith_hoist_info.len(),
            arith_hoist_info
                .iter()
                .map(|h| (h.seq_start, h.seq_end, h.steps.len()))
                .collect::<Vec<_>>(),
        );
    }

    // T5.2.1 — SCEV induction variable analysis.
    //
    // Produces an `InductionVar` entry per detected counted loop. The
    // result is stored on the Compiler so downstream passes (unrolling,
    // vectorization, range-check elimination) can query stride, bound,
    // and trip count without re-walking the bytecode.
    let induction_vars = crate::scev::analyze_induction_variables(code, code_len, &loops);

    // T5.2.14 — Null-check elimination dataflow.
    //
    // Walks the bytecode once and produces a per-PC bitmask of locals
    // proven non-null. Future null-check emission paths consult this
    // via `Compiler::is_local_nonnull(pc, local)` to skip redundant
    // `TEST reg, reg; JZ throw_npe` sequences.
    //
    // The receiver seed is derived here rather than passed in — see
    // `null_check_elim::receiver_in_local_zero` for why, and for what the
    // `None` (the two numbers disagree) case protects.
    let null_check_info = {
        let receiver = super::null_check_elim::this_nonnull_enabled()
            && crate::null_check_elim::receiver_in_local_zero(method_key, num_params)
                .unwrap_or(false);
        crate::null_check_elim::analyze_with_receiver(code, code_len, receiver)
    };

    // BCE: analyze loops for bounds check elimination
    // DBG (env-gated): CRATONVM_JIT_NO_BCE disables bounds-check elimination
    // (and SIMD, which also elides per-element checks) so every array access is
    // bounds-checked — to test whether an elided check causes the out-of-bounds
    // array-store heap corruption.
    let no_bce = cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_BCE");
    let (bounds_safe_pcs, speculative_bce_guards) = if no_bce {
        (FxHashSet::default(), Vec::new())
    } else {
        // The handler table is REQUIRED by the guard-dominated range reason: a
        // flow-sensitive fact is a claim about every way control reaches a
        // point, and an exception edge is one of those. Without the table that
        // reason refuses outright. `exception_ranges` here is the shadowed
        // output-coordinate copy, which is the space BCE works in.
        analyze_bounds_elimination_with_handlers(code, code_len, &loops, Some(&exception_ranges))
    };

    // Guarded matrix dot-product lowering.  This is a pre-header replacement
    // like the SIMD reductions below, but it remains useful for Java's
    // array-of-row `int[][]` layout where the right-hand column is not
    // contiguous and therefore cannot use ordinary packed loads.  The kill
    // switch restores the generic scalar emitter for diagnostics.
    let matrix_dot_enabled =
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_MATRIX_DOT").map_or(true, |v| {
            let value = v.trim();
            value != "0"
                && !value.eq_ignore_ascii_case("false")
                && !value.eq_ignore_ascii_case("off")
        });
    let matrix_dot_loops: Vec<MatrixDotLoop> = if matrix_dot_enabled && !no_bce {
        loops
            .iter()
            .filter(|(header, _)| !bypassable_headers.contains(header))
            .filter_map(|&(header, back_edge)| {
                let loop_end = back_edge + bytecode_len_at(code, back_edge);
                let iv = find_induction_variable(code, header, loop_end)?;
                detect_matrix_dot_loop(code, header, back_edge, iv)
            })
            .collect()
    } else {
        Vec::new()
    };
    if !matrix_dot_loops.is_empty()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN")
    {
        eprintln!(
            "[JIT_GEN] matrix-dot headers={:?}",
            matrix_dot_loops
                .iter()
                .map(|dot| dot.header_pc)
                .collect::<Vec<_>>()
        );
    }

    // SIMD: detect vectorizable int-array-sum loops (requires AVX2)
    let simd_loops = if has_avx2() && !no_bce {
        let mut simd = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_sum(code, header, back_edge, iv) {
                    simd.push(info);
                }
            }
        }
        simd
    } else {
        Vec::new()
    };

    // No vectorised floating-point reduction. There used to be one for
    // `s += a[i]` over a `double[]`: four lane accumulators seeded with +0.0
    // and combined as (l0+l2)+(l1+l3). Java FP addition is strict IEEE and not
    // associative, so that reordering changed results ({1e16, 1, -1e16, 1}
    // summed to 2.0 instead of 1.0) and turned an all -0.0 sum into +0.0.
    // `vector_gate::admit_vectorization` refuses reductions under
    // `FpRelaxation::Strict` for exactly this reason; the retired detector
    // never consulted it. A future FP reduction goes through that gate.

    // T5.2.15 — Int-array element-wise SIMD detection.
    //
    // Unlike reduction, detection here is *unconditional on AVX2* so
    // the information is available to any downstream pass (e.g.
    // cost-based vectorization, auto-tuning). Emission code checks
    // `has_avx2()` before issuing AVX2-only encodings.
    let simd_element_wise_loops = {
        let mut ewise = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_element_wise(code, header, back_edge, iv) {
                    ewise.push(info);
                }
            }
        }
        ewise
    };
    // One call, not three inlined copies: the optimizing tier's admission
    // chain asks the same question through
    // `single_pass_has_bulk_byte_lowering`, and it has to get the same answer
    // this does or it will hand the IR tier a method this backend vectorises.
    let BulkByteLoops {
        zero_fill: bulk_zero_byte_fill_loops,
        set_stride: bulk_set_byte_stride_loops,
        sieve: byte_sieve_loops,
    } = detect_bulk_byte_loops(code, code_len, &loops, &bypassable_headers);
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN")
        && !(bulk_zero_byte_fill_loops.is_empty()
            && bulk_set_byte_stride_loops.is_empty()
            && byte_sieve_loops.is_empty())
    {
        eprintln!(
            "[JIT_GEN] bulk-byte headers: zero-fill={:?} set-stride={:?} sieve={:?}",
            bulk_zero_byte_fill_loops
                .iter()
                .map(|f| f.header_pc)
                .collect::<Vec<_>>(),
            bulk_set_byte_stride_loops
                .iter()
                .map(|f| f.header_pc)
                .collect::<Vec<_>>(),
            byte_sieve_loops
                .iter()
                .map(|s| s.header_pc)
                .collect::<Vec<_>>(),
        );
    }

    // Loop unrolling: detect small loops suitable for unrolling
    // PGO: use profiled trip counts to guide unroll factor when available.
    // Static heuristic fallback:
    //   Body ≤20 bytecodes  → 4x unroll (3 extra copies)
    //   Body 20-50 bytecodes → 2x unroll (1 extra copy)
    //   Body > 50            → no unroll (unless PGO says otherwise, up to 100 bytes)
    //
    // The size band is a PROFITABILITY heuristic and nothing else. Every
    // LEGALITY question — reducibility, single entry, inner-cycle
    // reducibility, branches to the back edge, handler containment,
    // pre-header bypass, time-to-safepoint — is answered by
    // `plan_native_unroll`, which gates every entry that reaches
    // `compiler.unroll_loops`. It must stay the only producer of this vector:
    // the emitter's `0xa7` arm treats membership as proof that duplicating
    // the body's machine code is sound. See its doc comment for what the old
    // `code[back_edge] == 0xa7` + body-size test was missing.
    //
    // `loop_xform.is_some()` is redundant with `!native_unroller_enabled()`
    // (arming the rewriter is what turns the native unroller off, and a
    // transform can only exist when armed), and is written anyway: this
    // vector is the emitter's proof that duplicating machine code is sound,
    // and "the bytecode was already duplicated" must be visible AT the vector
    // rather than two functions away.
    let unroll_loops: Vec<(usize, usize, usize)> =
        if loop_xform.is_some() || !native_unroller_enabled() {
            Vec::new()
        } else {
            loops
                .iter()
                .filter_map(|&(header, back_edge)| {
                    // Only unroll loops with goto back-edge (not conditional)
                    if back_edge >= code_len || code[back_edge] != 0xa7 {
                        return None;
                    }
                    let body_size = back_edge - header;
                    if body_size < 5 {
                        return None;
                    }

                    // PGO path: use profiled trip count if available for this back-edge
                    if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
                        // `saturating_sub`: the old `pgo_factor - 1` underflowed on
                        // a 0 hint. A 0/1 factor now means "no extra copies", which
                        // `plan_native_unroll` refuses as `TooManyCopies`.
                        let extra_copies = pgo_factor.saturating_sub(1);
                        // PGO extends unrolling eligibility to larger loops (up to 100 bytes)
                        if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                            return plan_native_unroll(
                                code,
                                code_len,
                                header,
                                back_edge,
                                extra_copies,
                                &exception_ranges,
                                &bypassable_headers,
                            );
                        }
                    }

                    // Static heuristic fallback
                    let extra_copies = if body_size <= 20 {
                        3 // 4x unroll
                    } else if body_size <= 50 {
                        1 // 2x unroll (covers FP-heavy loops like N-Body advance)
                    } else {
                        return None;
                    };
                    plan_native_unroll(
                        code,
                        code_len,
                        header,
                        back_edge,
                        extra_copies,
                        &exception_ranges,
                        &bypassable_headers,
                    )
                })
                .collect()
        };
    if !unroll_loops.is_empty() && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
        eprintln!("[JIT_GEN] unroll admitted={unroll_loops:?}");
    }

    // FP LICM: detect loop-invariant FP loads to hoist
    let fp_hoist_info = find_fp_loop_hoists(code, code_len, &loops);
    let fp_hoist_info: Vec<FpLoopHoist> = fp_hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();

    // FP strength reduction: detect dmul-by-2.0 → dadd-self inside loops
    let fp_strength_reduction_pcs =
        find_fp_strength_reductions(code, code_len, &loops, &ldc2w_info);

    // Register allocation: graph-coloring allocator for locals.
    //
    // A method compiled with precise exceptional frames has its handler frame
    // rebuilt from REGISTER homes, so the interference graph must know that
    // protected code can branch to the handler — otherwise a local only the
    // catch block reads is dead throughout the try and shares its register with
    // something else. Every other compile passes no handlers and is unchanged.
    let ra_handlers: &[(usize, usize, usize)] = if precise_exception_frames {
        &exception_ranges
    } else {
        &[]
    };
    // `param_jvm_slots` (not just `num_params`): a category-2 parameter spans
    // two JVM slots, so the allocator must know which slots actually hold an
    // incoming argument. See `regalloc::param_live_in_mask`.
    let alloc_result = crate::regalloc::allocate_registers_with_handlers(
        code,
        code_len,
        max_locals,
        num_params,
        param_jvm_slots,
        &loops,
        ra_handlers,
    );

    // Pure-kernel GPR local homes (see `kernel_reg_locals_enabled` for the
    // full safety argument). Consume the per-compile request (set only by the
    // method-entry compile path) so it can never leak into a later compile,
    // then engage only for the pure-kernel shape: no calls of any kind, no
    // field/static ops, no allocation, no typechecks, no inline sites, and no
    // speculative BCE guards (those deopt with frame-stashed state). Reference
    // locals are masked back to frame homes, so GC visibility is unchanged.
    let pure_kernel = (kernel_reg_homes_requested || kernel_reg_homes_osr_requested)
        && kernel_reg_locals_enabled()
        && invoke_info.is_empty()
        && direct_calls.is_empty()
        && mic_slots.is_empty()
        && pic_slots.is_empty()
        && indy_info.is_empty()
        && field_info.is_empty()
        && static_field_info.is_empty()
        && new_info.is_empty()
        && new_deferred_info.is_empty()
        && anewarray_info.is_empty()
        && anewarray_deferred_info.is_empty()
        && multianewarray_info.is_empty()
        && typecheck_info.is_empty()
        && compact_field_info.is_empty()
        && inline_sites.is_empty()
        && speculative_bce_guards.is_empty();
    // When precise-map general register homes are already enabled, this pure
    // kernel does not need the narrow allocator to turn homes on again. It is
    // still a pure kernel, however, and therefore remains eligible for the
    // call-free deferred operand cache captured by `Compiler::new`.
    let kernel_reg_homes = pure_kernel && !callee_saved_gpr_local_homes_enabled();
    let mut alloc_result = if kernel_reg_homes {
        let mut ar = alloc_result;
        let ref_mask =
            crate::regalloc::find_reference_locals(code, code_len, max_locals) | param_oop_mask;
        for (i, assignment) in ar.assignments.iter_mut().enumerate() {
            if i >= 64 || (ref_mask >> i) & 1 == 1 {
                *assignment = None;
            }
        }
        // Recompute the save/restore set from the surviving assignments so
        // the prologue/epilogue and frame sizing stay consistent.
        let mut used: Vec<u8> = ar.assignments.iter().flatten().copied().collect();
        used.sort_unstable();
        used.dedup();
        ar.used_callee_saved = used;
        ar
    } else {
        alloc_result
    };
    if !matrix_dot_loops.is_empty() {
        // R12..R15 are private scratch homes for the tight pre-header.  Do not
        // let graph coloring simultaneously assign a Java local to one of
        // them; locals displaced here simply retain their canonical frame
        // homes.  Compiler::new still saves all four registers for ABI
        // correctness, independently of the local-home diagnostic gates.
        for assignment in &mut alloc_result.assignments {
            if matches!(*assignment, Some(R12 | R13 | R14 | R15)) {
                *assignment = None;
            }
        }
    }

    // Precise escape re-analysis. `jit_scan` produced `non_escaping_new`
    // with a conservative empty shape map (it has no CP resolver). Now
    // that `invoke_info` carries every invokespecial's resolved
    // descriptor, rebuild the shape map and re-run `analyze_escapes`.
    // This lets a trivial `new; dup; invokespecial <init>()V` keep its
    // scalar-replacement eligibility while an arg-bearing constructor
    // (`<init>(I)V`, …) correctly escapes its receiver — the latter
    // initializes fields in a separate, un-inlined method body that the
    // JIT frame cannot reproduce. Skipping this re-analysis (or running
    // it without descriptors) caused boxed values to come back as 0
    // (the `Integer.valueOf` / `String.toLowerCase` archetype).
    //
    // SR-reachability fix (real-frame-deopt x64 backport, Phase B prerequisite):
    // the precise re-analysis here is the AUTHORITATIVE escape analysis — it
    // recomputes from scratch with the resolved invokespecial shapes and does not
    // use `non_escaping_new` as a seed. Gate it on whether the method has any
    // `new` allocation (`new_info`), NOT on whether `jit_scan`'s conservative
    // pre-pass found a non-escaping object. `jit_scan` runs `analyze_escapes` with
    // an EMPTY shape map, whose `None` arm `escape_all!`s every invokespecial
    // receiver — so it returns an empty `non_escaping_new` for the ubiquitous
    // `new X(); <init>()V` pattern, and the old `if non_escaping_new.is_empty()`
    // gate then skipped the precise pass that WOULD recognize it. Net effect of
    // that bug: single-pass scalar replacement never fired for ordinary
    // allocations at runtime. Gating on `new_info` instead lets it fire (and is
    // what makes the Phase B `VirtualObject` deopt path reachable).
    let non_escaping_new: std::collections::HashSet<usize> = if new_info.is_empty() {
        non_escaping_new
    } else {
        let mut invokespecial_shapes: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
        for &(ipc, info_ptr) in &invoke_info {
            // SAFETY: `info_ptr` comes from `invoke_info`, whose entries
            // are kept live by the caller for the whole compilation.
            let info = unsafe { &*info_ptr };
            if info.invoke_kind == 1 {
                // The descriptor is NOT evidence of an empty body — see
                // `elidable_init_pcs`. Only a pc the caller's resolver proved
                // may be treated as a no-op here; everything else falls into
                // `analyze_escapes`'s arg-bearing arm, which escapes the
                // receiver and so keeps both the allocation and the call.
                let proven_empty_init = elidable_init_pcs
                    .as_ref()
                    .is_some_and(|pcs| pcs.contains(&ipc));
                invokespecial_shapes.insert(
                    ipc,
                    InvokeSpecialShape {
                        arg_slots: info.num_jit_args,
                        is_trivial_void_init: proven_empty_init,
                    },
                );
            }
        }
        analyze_escapes(code, code_len, &invokespecial_shapes)
    };
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SCALAR_DEOPT") && !new_info.is_empty() {
        eprintln!(
            "[DBG_SCALAR_DEOPT] x64::compile escape re-analysis: new_info={} non_escaping_new={:?} invoke_info={}",
            new_info.len(),
            { let mut v: Vec<usize> = non_escaping_new.iter().copied().collect(); v.sort(); v },
            invoke_info.len(),
        );
    }

    // Scalar replacement: plan frame-local storage for non-escaping object fields
    let num_hoists = hoist_info.len();
    let scalar_base = max_locals + (if needs_heap { 1 } else { 0 }) + num_hoists;
    let empty_non_escaping = std::collections::HashSet::new();
    let non_escaping_for_sr = if precise_exception_frames
        || cratonvm_types::flags::runtime_flag_on("CRATONVM_DISABLE_SCALAR_REPLACEMENT")
    {
        &empty_non_escaping
    } else {
        &non_escaping_new
    };
    let sr_plan = plan_scalar_replacement(
        code,
        code_len,
        non_escaping_for_sr,
        &new_info,
        &invoke_info,
        scalar_base,
    );
    let num_scalar_slots = sr_plan.total_slots;
    let force_inline_new =
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_ENABLE_INLINE_NEW");
    let cache_jit_thread_for_inline_new = needs_heap
        && helpers.get_current_thread != 0
        && helpers.tlab_post_init != 0
        && helpers.new_object != 0
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DISABLE_INLINE_NEW")
        && new_info
            .iter()
            .any(|(_, _, num_fields, has_prim_init, has_finalizer)| {
                HEADER_SIZE + num_fields.saturating_mul(SLOT_SIZE) <= 256
                    && ((!*has_prim_init && !*has_finalizer) || force_inline_new)
            });
    // Inline self-recursion stack check: a raw self-call site is an
    // `invokestatic` pc with neither an invoke-info entry nor a direct-call
    // plan (the exact condition the 0xb8 arm's "Self-recursive call"
    // else-branch keys on -- `try_compile` deliberately skips creating invoke
    // metadata for them). When one exists, reserve the floor frame slot so
    // each such site can do the two-instruction `CMP RSP, [rbp - floor]`
    // instead of a `self_call_stack_guard` helper CALL per recursion level.
    // The walk uses `bytecode_len_at`; a desync past a variable-length switch
    // can at worst set the flag spuriously, which only reserves an unused
    // slot + one prologue helper call (never unsound).
    let reserve_stack_floor = needs_heap
        && helpers.self_call_stack_guard != 0
        && helpers.native_stack_floor_fn != 0
        && inline_self_guard_enabled()
        && {
            let mut found = false;
            let mut pc = 0usize;
            while pc < code_len {
                if code[pc] == 0xb8
                    && !invoke_info.iter().any(|(p, _)| *p == pc)
                    && !direct_calls.iter().any(|(p, _)| *p == pc)
                {
                    found = true;
                    break;
                }
                pc += bytecode_len_at(code, pc);
            }
            found
        };

    KERNEL_REG_HOMES_ACTIVE.with(|c| c.set(pure_kernel));
    let mut compiler = Compiler::new(
        method_key.to_string(),
        buf,
        max_locals,
        num_params,
        max_stack,
        needs_heap,
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        hoist_info,
        arith_hoist_info,
        array_len_hoist_info,
        alloc_result,
        !matrix_dot_loops.is_empty(),
        *helpers,
        num_scalar_slots,
        cache_jit_thread_for_inline_new,
        reserve_stack_floor,
        gc_inert_selfrec && reserve_stack_floor,
        precise_exception_frames,
        // The `0xba` lowering emits a frame-deopt stub that spills 32 registers
        // into the `SavedRegisters` region, and it does so whether or not
        // `deopt_real_enabled()`. The frame therefore has to reserve that region
        // on the same condition — see `deopt_regs_size` in `x64.rs`.
        !indy_info.is_empty(),
        protected_ranges,
    );
    KERNEL_REG_HOMES_ACTIVE.with(|c| c.set(false));
    // ── Compiled local exception handlers ────────────────────────────────
    //
    // Arm only where every prerequisite is a fact rather than a hope, because
    // the failure mode of getting one wrong is a `catch` block entered on a
    // frame that does not describe it:
    //
    //  * the helper is wired — without it there is nothing to ask;
    //  * `precise_exception_frames`, which is what puts the exception-table
    //    edges into the interference graph (`ra_handlers` above). A local
    //    homed in a callee-saved register keeps ONE home for the whole method,
    //    so entering a handler mid-method finds every local where the handler's
    //    code expects it — but only because that modelling stopped two
    //    simultaneously-live locals sharing a register across the protected
    //    range. It is also the flag under which a throwing site publishes the
    //    reason-9 frame this feature falls back to;
    //  * `needs_heap`, because the stub passes the hidden `SharedVm` pointer
    //    from `heap_local_offset` and without the flag that slot is `[rbp-0]`,
    //    the saved RBP. Every method with an `invoke*` sets it, which is every
    //    method that can throw into its own handler from a call;
    //  * no bytecode loop rewrite in effect — the staged table is in
    //    INTERPRETER bci space and a rewrite moves everything into output-pc
    //    space. `exception_ranges` gets remapped for the analyses; a handler
    //    ENTRY has no single image under a transform that copies loop bodies,
    //    so refuse rather than pick a copy.
    let local_handlers_armed = crate::local_handlers_enabled()
        && helpers.local_handler_lookup != 0
        && precise_exception_frames
        && needs_heap
        && loop_xform.is_none()
        && !staged_local_handlers.is_empty();
    crate::note_local_handlers_armed(local_handlers_armed);
    if local_handlers_armed {
        crate::metrics::note_local_handler(crate::metrics::LOCAL_HANDLER_METHOD_ARMED);
        compiler.local_handler_table = staged_local_handlers;
        compiler.local_handler_class_id = staged_local_handler_class;
    }
    // Safepoint publication plan (arch-2026-07-26 R1). Built here rather than
    // inside `Compiler::new` because it needs `code` and `param_oop_mask`,
    // neither of which that constructor receives. `compiler.local_assignments`
    // is final at this point — `Compiler::new` moved the (possibly
    // kernel-masked, possibly all-`None` when GPR local homes are disabled)
    // assignment vector into the struct and nothing mutates it afterwards — so
    // the plan describes exactly the register homes this compile will emit.
    //
    // `param_oop_mask` is unioned in for the case `find_reference_locals`
    // cannot see: a reference PARAMETER that the method never `aload`s. Without
    // it such a local would look primitive and could be left unpublished while
    // genuinely holding an oop.
    //
    // COST GATE. `plan_safepoint_publication` runs `live_locals_per_pc_with_
    // coverage`, a second whole-method liveness pass on top of the one
    // `allocate_registers` just did. R1 consumes only `no_reference_in_registers()`
    // and never touches the liveness-narrowed `publish_at` vector, so paying for
    // it on every compile would be a JIT-compile-time regression inside a change
    // whose entire purpose is a speedup — and would confound measuring it.
    //
    // Skip it whenever no local has a register home at all: there the plan
    // provably cannot change the answer (`register_homed_reference_locals` would
    // be `0` ⇒ `no_reference_in_registers()` ⇒ `false`, which is exactly what
    // the `None` fallback's `any(Option::is_some)` also yields), so leaving the
    // plan absent is behaviour-identical at zero cost. The methods that DO have
    // register homes are precisely the population R1 exists to speed up.
    //
    // FOLLOW-UP: R2 needs `publish_at`, so it will need the pass unconditionally.
    // Before landing R2, `regalloc` should grow a `publish_always`-only
    // constructor that skips the liveness walk, or thread `allocate_registers`'
    // existing liveness result through instead of recomputing it.
    if compiler.local_assignments.iter().any(Option::is_some) {
        // Bound separately: `compiler.a = f(&compiler.b)` borrows and assigns
        // the same struct in one statement, which is needlessly close to the edge.
        let safepoint_publish = crate::regalloc::plan_safepoint_publication(
            code,
            code_len,
            max_locals,
            num_params,
            param_jvm_slots,
            &compiler.local_assignments,
            param_oop_mask,
            ra_handlers,
        );
        compiler.safepoint_publish = Some(safepoint_publish);
    }
    compiler.param_jvm_slots = param_jvm_slots.to_vec();
    compiler.param_slot_span = param_slot_span;
    compiler.method_key = method_key.to_string();
    compiler.despec = despec.cloned();
    // Coordinate change, emitter half: the three sites that BAKE a bci as an
    // immediate into machine code consult this. It must be installed before
    // `compile_bytecode` runs — those stubs are emitted at the end of that
    // call, so no post-pass over the finished `CompiledMethod` could reach
    // them. `None` (the identity) on every unarmed compile.
    compiler.bci_provenance = loop_xform.as_ref().map(|x| x.bci_of.clone());
    // …and which of those output pcs are an image of nothing. See the field.
    compiler.synthetic_guard_span = loop_xform.as_ref().and_then(|x| x.guard_span());
    // deopt-osr Step 9 follow-up (c): per-bci de-spec. Drop any speculative-BCE
    // guard whose loop header was recorded in the de-spec registry (a guard that
    // repeatedly deopted past the per-bci give-up threshold). Those headers fall
    // back to per-access bounds checks instead of the speculative elide, so the
    // method stays compiled (no whole-method blacklist) but no longer re-makes
    // the failed speculation. Dropping a guard MUST also drop the elisions it
    // justified: each guard's `covered_pcs` are removed from `bounds_safe_pcs`
    // so those accesses get their per-element checks back — a dropped guard
    // with the elisions left in place would be an UNGUARDED speculative elide
    // (silent out-of-bounds access on exactly the input that kept deopting).
    // Inert on the `compile()` wrapper and with nothing de-spec'd:
    // `DespecRegistry::contains` returns `false` for an empty key or empty
    // registry, and `None` consults nothing, so both sets are unchanged ⇒
    // byte-identical codegen.
    let mut bounds_safe_pcs = bounds_safe_pcs;
    let speculative_bce_guards: Vec<SpeculativeBCEGuard> = speculative_bce_guards
        .into_iter()
        .filter(|g| {
            let despec = despec
                .is_some_and(|registry| registry.contains(method_key, despec_bci(g.loop_header)));
            if despec {
                for covered_pc in &g.covered_pcs {
                    bounds_safe_pcs.remove(covered_pc);
                }
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                    eprintln!(
                        "[cratonvm-deopt] de-spec: suppressing speculative-BCE guard at \
                         loop_header bci={} for {} (recompile without it; per-element \
                         checks restored at {:?})",
                        g.loop_header, method_key, g.covered_pcs
                    );
                }
            }
            !despec
        })
        .collect();
    // Same obligation as the de-spec drop above, for a header whose pre-header
    // an outside-the-loop branch can skip: the guard would not have run, so the
    // elisions it justified must go back to per-access checks (an elide with no
    // guard is a silent out-of-bounds access).
    let speculative_bce_guards: Vec<SpeculativeBCEGuard> = speculative_bce_guards
        .into_iter()
        .filter(|g| {
            let bypassable = bypassable_headers.contains(&g.loop_header);
            if bypassable {
                for covered_pc in &g.covered_pcs {
                    bounds_safe_pcs.remove(covered_pc);
                }
            }
            !bypassable
        })
        .collect();
    compiler.bounds_safe_pcs = bounds_safe_pcs;

    // SIMD gating: a SIMD loop transform replaces the per-element accesses of
    // `arr[i]` for `i` in `[entry_iv, bound)` with an UNCHECKED batch loop, so
    // it carries the same proof obligation as a BCE elision — for EVERY array
    // it touches, `bound <= arr.length` (plus a non-negative start index) must
    // be established either statically (the bound provably IS that array's
    // length and the IV provably starts >= 0) or by a speculative loop-header
    // guard that survived de-spec (the guard also tests `iv >= 0`, and is
    // emitted before the SIMD preheader). An uncovered array — e.g. the OUT
    // store of `out[i] = a[i] + b[i]` in a loop bounded by `a.length` when
    // `out` is shorter — would batch-store past the array end with no
    // exception (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md;
    // before this gate, `detect_int_array_element_wise` candidates vectorized
    // with no coupling to the bounds analysis at all).
    // (`no_bce` also lands here: CRATONVM_JIT_NO_BCE always claimed to disable
    // "BCE and SIMD", but only the int-sum detection was actually gated on it —
    // the FP-sum and element-wise transforms kept vectorizing with elided
    // checks. Routing every SIMD candidate through this coverage check makes
    // the debug gate true to its documentation.)
    let simd_covered = |header: usize, arr_local: usize, bound_local: usize, iv_local: usize| {
        !no_bce
            && ((find_bound_arraylength_provenance(code, code_len, bound_local) == Some(arr_local)
                && find_iv_nonneg_start(code, code_len, iv_local))
                || speculative_bce_guards.iter().any(|g| {
                    g.loop_header == header
                        && g.array_local == arr_local
                        && g.bound_local == bound_local
                }))
    };
    let simd_loops: Vec<SimdIntArraySum> = simd_loops
        .into_iter()
        .filter(|s| simd_covered(s.header_pc, s.array_local, s.bound_local, s.iv_local))
        .collect();
    let simd_element_wise_loops: Vec<SimdArrayElementWise> = simd_element_wise_loops
        .into_iter()
        .filter(|e| {
            simd_covered(e.header_pc, e.out_local, e.bound_local, e.iv_local)
                && simd_covered(e.header_pc, e.a_local, e.bound_local, e.iv_local)
                && simd_covered(e.header_pc, e.b_local, e.bound_local, e.iv_local)
        })
        .collect();
    // A SIMD batch pre-header is emitted under the same placement contract as
    // the LICM hoists, so a bypassable header must not carry one either.
    let simd_loops: Vec<SimdIntArraySum> = simd_loops
        .into_iter()
        .filter(|s| !bypassable_headers.contains(&s.header_pc))
        .collect();
    let simd_element_wise_loops: Vec<SimdArrayElementWise> = simd_element_wise_loops
        .into_iter()
        .filter(|e| !bypassable_headers.contains(&e.header_pc))
        .collect();
    // Index the speculative guards by loop-header PC once, so the per-header
    // emit loop does an O(1) map lookup instead of an O(guards) filtered scan
    // at every loop header.
    {
        let mut by_header: FxHashMap<usize, Vec<SpeculativeBCEGuard>> = FxHashMap::default();
        for g in &speculative_bce_guards {
            by_header.entry(g.loop_header).or_default().push(g.clone());
        }
        compiler.speculative_bce_guards_by_header = by_header;
    }
    compiler.speculative_bce_guards = speculative_bce_guards;
    compiler.compact_field_off = compact_field_info
        .into_iter()
        .map(|(pc, off, is_ref)| (pc, (off, is_ref)))
        .collect();
    compiler.new_info = new_info;
    compiler.new_deferred_info = new_deferred_info;
    compiler.anewarray_info = anewarray_info;
    compiler.anewarray_deferred_info = anewarray_deferred_info;
    compiler.invoke_info = invoke_info;
    // Index the call-site argument tags before the walk: the deopt snapshots
    // built during it consult the index by bci. See `invoke_stack_arg_types`.
    compiler.index_invoke_arg_types();
    compiler.indy_info = indy_info;
    compiler.direct_calls = direct_calls;
    // deopt-osr Step 8 (test trigger): under CRATONVM_OSR_EXIT_TEST + CRATONVM_DEOPT_REAL,
    // pick the first (lowest-pc) detected loop header as the synthetic OSR-exit
    // branch site. `None` in production (either gate off) ⇒ no trigger emitted ⇒
    // byte-identical code. `detect_loops` returns (header, end) pairs.
    //
    // P4 (Step 8 follow-up): `CRATONVM_OSR_EXIT_AFTER=N` also arms the trigger at the
    // same loop header, but counter-gated (`osr_exit_after_count = Some(N)`) so the
    // JIT advances ~N iterations before the exit — exercising the true OSR-exit
    // transfer with genuinely advanced state. Either gate (both require DEOPT_REAL)
    // selects the bci; AFTER takes precedence over TEST for the emitted form.
    compiler.osr_exit_after_count = if crate::deopt_real_enabled() {
        crate::osr_exit_after()
    } else {
        None
    };
    compiler.osr_exit_test_trigger_bci = if crate::deopt_real_enabled()
        && (crate::osr_exit_test_enabled() || compiler.osr_exit_after_count.is_some())
    {
        loops.iter().map(|&(h, _)| h).min()
    } else {
        None
    };
    // deopt-osr: the through-JIT deopt-EXIT differential trigger. Under
    // CRATONVM_DEOPT_EAGER + DEOPT_REAL, force a reason-2 deopt-EXIT at the first
    // (lowest-pc) loop header. `None` (either gate off / no loop) ⇒ byte-identical.
    compiler.deopt_eager_bci = if crate::deopt_real_enabled() && crate::deopt_eager_enabled() {
        loops.iter().map(|&(h, _)| h).min()
    } else if crate::deopt_real_enabled() {
        // Phase B e2e: `CRATONVM_DEOPT_EAGER_BCI=<n>` points the eager deopt-EXIT
        // at a specific straight-line bci (where a scalar object is live in a
        // local), the only way to drive `VirtualObject` resume through the JIT.
        // Fire ONLY in a method that actually has a scalar object live at that
        // bci — so the global env doesn't perturb unrelated methods (e.g. the
        // driver `main`, which has no scalar replacement).
        crate::deopt_eager_bci_override().filter(|n| compiler.sr_local_prov_at.contains_key(n))
    } else {
        None
    };
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SCALAR_DEOPT")
        && !compiler.scalar_replaced.is_empty()
    {
        let mut keys: Vec<usize> = compiler.sr_local_prov_at.keys().copied().collect();
        keys.sort();
        eprintln!(
            "[DBG_SCALAR_DEOPT] x64 single-pass compile: scalar_replaced={} sr_local_prov_at_pcs={:?} eager_bci={:?} elided_monitor={}",
            compiler.scalar_replaced.len(),
            keys,
            compiler.deopt_eager_bci,
            compiler.has_elided_monitor,
        );
    }
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
        eprintln!(
            "[JIT_GEN_INSTALL] mic_slots count={} pcs={:?}",
            mic_slots.len(),
            mic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.mic_slots = mic_slots;
    // HIGH-7 — Inline 4-way PIC fast-path wiring (now active).
    //
    // The codegen in `Compiler::compile_op_invokevirtual` (search
    // `pic_inline`) keys off `compiler.pic_slots`. With the
    // `pic_slots` parameter now threaded through, callers that
    // eagerly allocate a `Box<JitPICSlot>` per polymorphic call
    // site (see `jit/src/lib.rs::try_compile`) activate the inline
    // cascade. Slots start empty (class_id == 0 at all 4 entries),
    // so the CMP cascade falls straight through to the helper on
    // first invocation; once the runtime helper populates a slot,
    // subsequent dispatches take the inline fast path.
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
        eprintln!(
            "[JIT_GEN_INSTALL] pic_slots count={} pcs={:?}",
            pic_slots.len(),
            pic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.pic_slots = pic_slots;
    compiler.unroll_loops = unroll_loops;
    compiler.simd_loops = simd_loops;
    compiler.matrix_dot_loops = matrix_dot_loops;
    // The pure-kernel deferred cache owns R8/R9 across bytecodes, while the
    // matrix-dot preheader uses those registers for its batch limit and
    // wrapping accumulator. Keep upstream's pure-kernel local homes, but
    // disable only the conflicting operand cache for this exact lowering.
    if !compiler.matrix_dot_loops.is_empty() {
        compiler.kernel_operand_cache = false;
    }
    compiler.branch_hints = branch_hints.into_iter().collect();
    compiler.loop_unroll_hints = loop_unroll_hints.into_iter().collect();
    compiler.ldc_info = ldc_info;
    compiler.ldc_string_info = ldc_string_info;
    compiler.ldc_class_info = ldc_class_info;
    compiler.ldc2w_info = ldc2w_info;
    compiler.ldc_fp_pcs = ldc_fp_pcs;
    compiler.fp_hoist_info = fp_hoist_info;
    compiler.fp_strength_reduction_pcs = fp_strength_reduction_pcs;
    // Phase B (real-frame-deopt x64 backport): build the per-field type map for
    // scalar-replaced objects by joining the per-access-site `field_info`
    // (`(pc, field_index, type_tag)`) with the plan's `field_ops` (`pc → new_pc`).
    // Each accessed field of a scalar object thus learns its JVM type tag, which
    // drives the `VirtualObject` field `FrameValue` width/ref-ness on deopt. Built
    // before `field_ops` is moved into `scalar_field_ops` below.
    {
        let mut sr_field_types: FxHashMap<(usize, usize), u8> = FxHashMap::default();
        for &(pc, field_index, type_tag) in &compiler.field_info {
            if let Some(&new_pc) = sr_plan.field_ops.get(&pc) {
                sr_field_types.insert((new_pc, field_index), type_tag);
            }
        }
        compiler.sr_field_types = sr_field_types;
    }
    compiler.sr_local_prov_at = sr_plan.local_prov_at;
    compiler.sr_monitor_at = sr_plan.monitor_at;
    compiler.sr_monitor_scalar_ops = sr_plan.monitor_scalar_ops;
    // `CRATONVM_DBG_JIT_EA=<method-key substring>` -- what escape analysis
    // decided for this method, per pc.
    //
    // A scalar-replaced `new` pushes a DUMMY ZERO and its field ops are
    // rewritten to frame slots; a site the plan covers and one it does not pop
    // DIFFERENT numbers of operands, so a `new` in `objects` whose `<init>`
    // is not in `init_skips` (or whose field ops are not in `field_ops`) leaves
    // the operand stack desynchronised for everything after it. In a method
    // that is one 23-arm `lookupswitch` of `new SQLxxx(); dup; invokespecial
    // <init>; areturn` -- `DataValueFactoryImpl.getNullDVDWithUCS_BASICcollation`
    // -- that is how one arm's constructor reaches another arm's object.
    if let Ok(want) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_EA") {
        let key = compiler.method_key.clone();
        if key.contains(&want) {
            let mut news: Vec<(usize, u32, usize)> = sr_plan
                .objects
                .iter()
                .map(|(pc, o)| (*pc, o.class_id, o.num_fields))
                .collect();
            news.sort_unstable();
            let mut fops: Vec<(usize, usize)> =
                sr_plan.field_ops.iter().map(|(a, b)| (*a, *b)).collect();
            fops.sort_unstable();
            let mut skips: Vec<usize> = sr_plan.init_skips.iter().copied().collect();
            skips.sort_unstable();
            let mut elid: Vec<usize> = elidable_init_pcs
                .as_ref()
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            elid.sort_unstable();
            let mut nen: Vec<usize> = non_escaping_new.iter().copied().collect();
            nen.sort_unstable();
            eprintln!(
                "[jit-ea] method={key} new_sites={} elidable_init_pcs={elid:?} \
non_escaping_new={nen:?} scalar_new={news:?} field_ops={fops:?} init_skips={skips:?}",
                compiler.new_info.len()
            );
        }
    }
    compiler.scalar_replaced = sr_plan.objects;
    compiler.scalar_field_ops = sr_plan.field_ops;
    compiler.scalar_init_skips = sr_plan.init_skips;
    compiler.inline_sites = inline_sites.into_iter().collect();
    compiler.inline_guard_variants = inline_guard_variants.into_iter().collect();
    // String call-site intrinsics: hand the resolved String field layout to
    // the compiler so intrinsic codegen can emit inline field loads.
    compiler.string_layout = string_layout;
    // T5.2.1 + T5.2.14 — transfer the pre-computed analyses.
    compiler.induction_vars = induction_vars;
    compiler.null_check_info = null_check_info;
    // T5.2.15 — element-wise SIMD detections.
    compiler.simd_element_wise_loops = simd_element_wise_loops;
    compiler.bulk_zero_byte_fill_loops = bulk_zero_byte_fill_loops;
    compiler.bulk_set_byte_stride_loops = bulk_set_byte_stride_loops;
    compiler.byte_sieve_loops = byte_sieve_loops;
    // Same R8/R9 ownership conflict the matrix-dot lowering has above: the
    // sieve preheader keeps the prime count in R8 and its word-scan temporary
    // in R9, and the strided-store preheader keeps the step in R9. Those are
    // exactly the pure-kernel deferred operand cache's two scratch registers.
    if !compiler.byte_sieve_loops.is_empty() || !compiler.bulk_set_byte_stride_loops.is_empty() {
        compiler.kernel_operand_cache = false;
    }

    // MED-4 / Fix 3 — pre-build pc-indexed lookup maps for the hot
    // codegen sites (getfield/putfield/invoke*/new/anewarray/ldc/…)
    // so each query is O(1) rather than scanning the Vec.
    compiler.build_pc_indices();

    // Stage 2 (precise oop maps) — forward "must be oop" local-variable
    // dataflow. Lets `emit_oop_map_for_safepoint` record the canonical frame
    // slots of register/memory locals that hold object references at each
    // safepoint (in addition to the operand-stack slots), so a moving GC has
    // precise, updatable coverage of every live oop. Behaviour-neutral on the
    // default (non-moving) path: `conservative_roots::scan_one_frame_precise`
    // already sweeps the whole frame region, so the extra precise entries are
    // redundant there and re-validated via `heap.is_object_address`.
    let (lo_masks, lo_reached, lo_windows) =
        compute_local_oop_masks_windowed(code, code_len, max_locals, param_oop_mask);
    compiler.local_oop_masks = lo_masks;
    compiler.local_oop_reached = lo_reached;
    // The layout the windowed readers need. `lo_windows == 0` is the REFUSAL
    // (`max_locals` past `MAX_WINDOWED_LOCALS`, or the switch off above 64),
    // and leaves `local_oop_masks` empty, which is what
    // `map_incomplete_cause::LOCAL_MASK_UNSUPPORTED` fails closed on.
    compiler.local_oop_windows = lo_windows;
    compiler.local_oop_stride = code_len;
    // The entry state, kept alongside the per-pc vectors: the method-entry
    // safepoint poll is at no bytecode pc, so it has nothing to look up.
    // See `Compiler::local_oop_mask_at_current_pc`.
    compiler.param_oop_mask = param_oop_mask;

    // deopt-osr P2 — per-local width/type source for the deopt snapshot. Only the
    // (gated) snapshot consumes it, so skip the scan entirely in production.
    if crate::deopt_real_enabled() || precise_exception_frames {
        compiler.local_kinds = classify_local_kinds(code, code_len, max_locals);
        // Resolve the `Ambiguous` votes per bci where control flow allows it.
        compiler.local_kinds_refined =
            refine_ambiguous_local_kinds(code, code_len, &compiler.local_kinds, &exception_ranges);
        // FU2 — method-level cat-2/FP gate for the operand-stack snapshot.
        compiler.uses_long_float_double = code_uses_long_float_double(code, code_len);
        // deopt-osr OSR-exit dead-local fix — see `local_liveness`'s doc comment.
        // The exception table MUST be modelled: these snapshots are taken at
        // pcs inside protected ranges, and a local only the handler reads is
        // otherwise computed dead exactly there.
        // `_all`, not `_with_handlers`: the latter answers for slots 0..63
        // only, and a method with more than 64 locals then cannot drop a dead
        // local above slot 63 from the snapshot — which publishes `Unsupported`
        // for it and costs the whole method its OSR entry. Window 0 of this is
        // bit-for-bit the old answer, so a method with 64 locals or fewer is
        // unchanged.
        let (liveness, covered, words) = crate::regalloc::live_locals_per_pc_all(
            code,
            code_len,
            num_params,
            param_jvm_slots,
            &exception_ranges,
            compiler.num_locals,
        );
        compiler.local_liveness = liveness;
        compiler.local_liveness_words = words;
        compiler.local_liveness_covered = covered;
        compiler.exception_ranges_dbg_len = exception_ranges.len();
    }

    // The operand stack's width source. Placed HERE, immediately before the
    // walk, because it reads per-pc metadata assigned across a long stretch of
    // this function: field and static-field types, and call arities from all
    // three of `indy_info` / `direct_calls` / `invoke_info`.
    //
    // Run it any earlier and the vectors it has not seen yet read as ABSENT,
    // which the analysis treats as an unmodelled call site and poisons on — so
    // the result is silently empty and every snapshot keeps the coarse
    // encoding. That is exactly what the first attempt did (placed right after
    // `invoke_info`, three of its five inputs were still empty): the suites
    // stayed green and the refusal count did not move at all, which is the
    // signature of an analysis that answered nothing rather than one that
    // answered wrong.
    compiler.analyze_stack_kinds(code, code_len);

    // Emit prologue
    compiler.emit_prologue();
    if compiler.failed {
        let (site, pc, op) = compiler
            .failed_site
            .unwrap_or(("singlepass-prologue", 0, 0));
        crate::note_jit_bail_site_at(site, pc, op);
        return None;
    }
    let entry_offset = 0; // prologue starts at offset 0
    compiler.body_entry_offset = compiler.buf.pos(); // offset right after prologue

    // Compile bytecode
    if !compiler.compile_bytecode(code, code_len) {
        // Publish the refusing bytecode to the caller's bail-site record too:
        // this line names the opcode but not the method, and `try_compile`'s
        // `compile-bail` line names the method but not the opcode. Neither is
        // a diagnosis on its own, and they are not even both printed on the
        // same run for an OSR/callee compile.
        //
        // A refusal raised through the `failed` FLAG names its own site and the
        // pc/op live when it was raised; the flag is only checked after the
        // whole dispatch loop, so `dbg_last_pc`/`dbg_last_op` would name
        // whatever instruction happened to be last instead.
        let (site, pc, op) = compiler.failed_site.unwrap_or((
            "singlepass-codegen",
            compiler.dbg_last_pc,
            compiler.dbg_last_op,
        ));
        crate::note_jit_bail_site_at(site, pc, op);
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!("[cratonvm-jitc] codegen-bail site={site} pc={pc} op=0x{op:02x}");
        }
        return None;
    }

    // Backstop for the implicit null check. Every site recorded by
    // `emit_trusted_oop_receiver_check_at` must have been bound to a recovery
    // address by `bind_implicit_null_recovery` before the walk ended. One left
    // pending means a receiver check was elided and the slow path it faults
    // into was never emitted — the site would run unguarded and its
    // NullPointerException would arrive as a SIGSEGV.
    //
    // That cannot happen through any path in the arm as written (the arm that
    // opts in always reaches its guarded slow path), which is exactly why it
    // is asserted rather than reasoned about: the property belongs to control
    // flow several hundred lines away from the elision, and an edit that
    // breaks it would produce a crash on a null receiver in production rather
    // than a red test.
    if compiler.has_unbound_implicit_null_sites() {
        crate::note_jit_bail_site_at("implicit-null-unbound", compiler.dbg_last_pc, 0);
        return None;
    }

    // Patch branches (both forward and backward are handled). A `false`
    // return means some branch targeted a PC that was never emitted as an
    // instruction boundary (malformed/unverified bytecode) — reject the
    // method rather than leave an unpatched jump in executable code.
    if !compiler.patch_branches() {
        // Name the target. `pc` here is the branch target with no native
        // offset and `op` the byte at it — enough to check against a `javap -c`
        // listing whether the target really is off-boundary (it usually is
        // not: see `jit-tailcall-swallows-shared-return-FIXED-20260803.md`).
        let (target, nearest) = compiler.unresolved_branch_target.unwrap_or((0, -1));
        let target_op = code.get(target).copied().unwrap_or(0);
        crate::note_jit_bail_site_at(
            "branch-target-not-an-instruction-boundary",
            target,
            target_op,
        );
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!(
                "[cratonvm-jitc] branch-target-unresolved target={target} op=0x{target_op:02x} \
                 nearest_emitted_at_or_below={nearest} code_len={code_len}"
            );
        }
        return None;
    }

    // Patch self-recursive calls to point to entry
    compiler.patch_self_calls(entry_offset);

    // Lazy-prologue perf lever: now that the body is fully compiled,
    // `shadow_pushed_any` is final — NOP out the prologue thread-fetch if no
    // register-resident oop was ever published (no-op when shadow is off).
    compiler.maybe_nop_out_shadow_fetch();

    // `estimated_size` is a heuristic; a pathological method can emit past it.
    // The emit hot path records the overflow instead of panicking — bail to
    // the interpreter here rather than returning a truncated, unsafe method.
    if compiler.buf.overflowed() {
        // A named codegen invariant break is NOT a sizing problem, and the two
        // used to be indistinguishable here: every `mark_overflowed` site — a
        // `rel8` displacement out of range, a frame offset with no ModRM form,
        // a deopt stub with no register-save area — landed in the branch below
        // and was reported as "code buffer estimate too small". Two costs, both
        // paid in production: the printed diagnostic named a cause that was not
        // the cause (with `wanted` UNDER `capacity`, contradicting itself), and
        // because the shortfall site is `try_compile`'s one bail-list exemption
        // the method was re-lowered in full on EVERY warmup-gate re-attempt,
        // failing identically each time and never becoming compiled.
        //
        // A bigger buffer cannot encode a displacement that has no encoding, so
        // these are permanent: name the reason and fall through to the ordinary
        // (bail-listed) refusal.
        if let Some(reason) = compiler.buf.codegen_failure_reason() {
            tracing::warn!(
                method = method_key,
                code_len = code_len,
                capacity = compiler.buf.capacity(),
                wanted = compiler.buf.wanted(),
                reason = reason,
                "JIT compile bailed: codegen invariant cannot be encoded; method stays interpreted"
            );
            crate::note_jit_bail_site(reason);
            return None;
        }
        // Name the method and the shortfall. A silent bail here is
        // indistinguishable from "the JIT chose not to compile this", which is
        // how a whole class of invoke-heavy methods came to stop being compiled
        // unnoticed (`resolvabletype-equals-jit-...`): the only
        // visible symptom was a flood of anonymous `try_patch_*: offset out of
        // bounds` warnings with no method attached to any of them.
        // The line itself is emitted BELOW, after the shortfall is recorded,
        // because its LEVEL depends on whether this attempt just spent the last
        // retry and `note_code_buffer_shortfall` is what bumps that count.
        //
        // Remember the shortfall so the NEXT attempt at this method sizes its
        // buffer from a measurement instead of the heuristic.
        //
        // This backend cannot retry in place — it consumes six one-shot
        // thread-local staging requests before the buffer is allocated, and
        // re-entering it here would find them gone. So the retry is deferred to
        // the next compile request, which arrives with those requests freshly
        // staged. What made that impossible before is that the caller treats
        // ANY backend-attempted `None` as permanent (`mark_jit_bail_listed`),
        // so the first overflow retired the method for the life of the process
        // — the estimate got exactly one chance and a method that needed more
        // was never compiled again. `try_compile` now exempts this one site.
        crate::note_code_buffer_shortfall(
            method_key,
            compiler.buf.wanted(),
            // The capacity that FAILED, so the next hint is strictly larger than
            // it. Without this the doubled `wanted` could land at or below the
            // heuristic, `estimated_size.max(hint)` re-allocated the same size,
            // and the retry was a bit-identical repeat — forever.
            compiler.buf.capacity(),
        );
        // Level split: DEBUG while the bail is RECOVERABLE, WARN once it is not.
        //
        // This line used to be `warn!` unconditionally, and it fired on every
        // boot of every process — `java/lang/CharacterData00.<clinit>:()V` on a
        // stock `Hello` — for a condition the VM handles by itself on the next
        // compile request. A warning that is always present is a warning nobody
        // reads, and it sat directly next to the `codegen_failure_reason()` arm
        // above, which is the one that really is permanent; keeping both at the
        // same level is what made the two indistinguishable in a log before the
        // reasons were split at all.
        //
        // Demoting is only safe because the bail stays COUNTED rather than
        // becoming silent, which is the failure this site's original comment
        // guards against: `note_jit_bail_site` below records it under
        // `CODE_BUFFER_TOO_SMALL_SITE` (reported per method by
        // `jit_bail_reason_for`), and `note_code_buffer_bail_cost` feeds the
        // `code_buffer_bails=N (discarded_compile_ms=M)` fields of
        // `tiered::dump_method_stats_to_stderr`. Nothing outside `docs/` parses
        // the message text (checked across `ci/`, `scripts/`, `tools/`,
        // `apps/`), so the wording of the recoverable arm is left alone for
        // those write-ups to keep matching.
        //
        // The exhausted arm is a genuinely new fact and stays at `warn!`: after
        // `MAX_CODE_BUFFER_RETRIES` doublings `try_compile` stops exempting this
        // site from the permanent bail list, so the method is now interpreted
        // for the life of the process and no later line will say so.
        if crate::code_buffer_retries_exhausted(method_key) {
            tracing::warn!(
                method = method_key,
                code_len = code_len,
                capacity = compiler.buf.capacity(),
                wanted = compiler.buf.wanted(),
                "JIT compile bailed: retry budget spent on a short code buffer; stays interpreted"
            );
        } else {
            tracing::debug!(
                method = method_key,
                code_len = code_len,
                capacity = compiler.buf.capacity(),
                wanted = compiler.buf.wanted(),
                "JIT compile bailed: code buffer estimate too small; retrying at the measured size"
            );
        }
        crate::note_jit_bail_site(crate::CODE_BUFFER_TOO_SMALL_SITE);
        crate::note_code_buffer_bail_cost(compile_started.elapsed());
        return None;
    }

    // The bytecode rewriter's coordinate change, CHECKED rather than assumed.
    //
    // `DeoptimizationPoint::bci` is what the VM resumes at, and it is recorded
    // deep inside the emitter from the emitter's own pc.
    // `build_and_record_deopt_point` publishes it through `Compiler::orig_bci`;
    // this re-derives that answer from the emitter pc each point kept
    // (`deopt_point_pcs`) and discards the METHOD if the translation did not
    // hold, if a point landed on the versioning guard's synthetic bytes, or if
    // two copies of one bytecode published disagreeing frames under one bci.
    // See the helper for why each of those is fatal.
    //
    // This replaces the "any deopt point at all discards the method" backstop
    // that stood here while `deopt_real`, precise exception frames and
    // `invokedynamic` were refused outright — a vector that was provably empty
    // then, and is the normal case now.
    if let Some(x) = &loop_xform {
        if let Err(why) = rewritten_deopt_points_are_publishable(
            x,
            &compiler.deopt_points,
            &compiler.deopt_point_pcs,
            orig_code_len,
        ) {
            crate::metrics::record_loop_xform_event("loop_xform_deopt_bci_unpublishable");
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
                eprintln!("[JIT_GEN] bytecode loop rewrite DISCARDED: {why}");
            }
            tracing::warn!(
                method = method_key,
                deopt_points = compiler.deopt_points.len(),
                reason = why.as_str(),
                "JIT compile bailed: bytecode loop rewrite could not publish its deopt \
                 points in interpreter-bci space; method stays interpreted"
            );
            return None;
        }
    }

    // Build the CompiledMethod with OSR metadata.
    //
    // `has_dispatch` gates the interpreter's compiled-entry fast path
    // (`interpreter.rs`: `if !compiled.has_dispatch { ... }`), which skips
    // `set_jit_thread`. That TLS pointer is what the dispatch helpers
    // (`jit_invoke_dispatch` / `jit_invoke_virtual_mic`) read via
    // `jit_thread_mut()`; when it is null they short-circuit and return 0.
    //
    // A method with `direct_calls` makes machine-level `CALL`s into other
    // compiled callees WITHOUT crossing a Rust boundary that could set the
    // TLS. Those callees inherit whatever `JIT_THREAD` the caller was
    // entered with, and may themselves dispatch (or transitively call a
    // method that does). So a method whose only inter-method calls are
    // direct calls must STILL be entered through the slow path that sets
    // `JIT_THREAD` — otherwise the callee's dispatch helper sees a null
    // thread and silently returns 0.
    //
    // This was the `Character.getType(char)` miscompile: its sole call,
    // `invokestatic Character.getType(int)`, resolved to a `direct_call`,
    // leaving `invoke_info` empty. The fast path skipped `set_jit_thread`,
    // so the directly-called `getType(int)` ran with a null thread and its
    // `CharacterData.of(...)` / `getType(...)` dispatches returned 0 →
    // `Character.getType` returned UNASSIGNED for every Latin-1 letter once
    // JIT-compiled.
    let has_dispatch = !compiler.invoke_info.is_empty()
        // A BRIDGED `invokedynamic` CALLS `jit_indy_bridge`, and that helper's
        // first act is `jit_thread_mut()` — it has to push the synthetic frame
        // the bootstrap runs on. A method whose only inter-method work is one
        // indy has an EMPTY `invoke_info`: an `invokedynamic` used to lower to
        // an uncommon trap, which calls nothing, so nothing on this list ever
        // saw it. `static String f(int i) { return "v=" + i; }` compiled with
        // `has_dispatch=false`, took the TLS-free fast entry, and the bridge
        // answered every call with a NULL STRING — the identical shape as the
        // `Character.getType` and `<clinit>`-gap entries above, and found the
        // same way, by a method small enough to have nothing else in it
        // (`probes/MinIndyProbe.java`).
        || compiler
            .indy_info
            .iter()
            .any(|&(_, _, _, _, bridge_site)| bridge_site != 0)
        || !compiler.direct_calls.is_empty()
        || !compiler.bounds_check_stubs.is_empty()
        || !compiler.null_check_store_stubs.is_empty()
        // RBC.6 — an athrow stashes a pending JIT exception; the
        // `!has_dispatch` fast entry paths return the raw value WITHOUT
        // draining it, which would leak the exception (and mis-read the
        // sentinel as a return value). Force the dispatch-aware route.
        || compiler.emitted_athrow
        // Live monitor helpers call `jit_thread_mut()` to identify the owner
        // and to enter GC-blocked parking on contention. A monitor-only method
        // otherwise looks call-free and would take the TLS-free fast entry.
        || compiler.emitted_monitor_call
        // A fallible `newarray` OOM bail needs the per-thread TLS set so the
        // helper can GC + construct the OOME (same rationale as direct_calls).
        || compiler.emitted_alloc_oom_check
        // Residual-6 companion fix — a failed checkcast stashes a CCE through
        // the JIT_THREAD TLS and bails with the i64::MIN sentinel; the entry
        // path must set the TLS and drain the pending exception.
        || compiler.emitted_checkcast_throw
        // A refused `aastore` stashes an ArrayStoreException the same way.
        || compiler.emitted_aastore_throw
        // `jit_instanceof` pins its receiver through the JIT thread while it
        // loads the target class; the thread-less entry left it unpinned.
        || compiler.emitted_instanceof_call
        // BUG-1 companion — a direct (non-dispatch) self-recursive CALL site:
        // its stack guard stashes a catchable StackOverflowError near native
        // exhaustion and returns the i64::MIN sentinel, so the method MUST be
        // entered through the dispatch-aware path that sets `JIT_THREAD` (the
        // guard constructs the SOE through it) and drains
        // `JIT_PENDING_EXCEPTION` on return. Without this, the register-only
        // fast entry mis-read the sentinel as a return value (int-truncated
        // to 0) and leaked the pending SOE (observed: DeepRec printed
        // "no-overflow r=0" instead of catching the error).
        || !compiler.self_call_patches.is_empty()
        // jit-clinit-gap-has-dispatch fix (2026-07-17): `jit_getstatic`,
        // `jit_putstatic_*`, and `jit_new_object` all now run
        // `ensure_class_initialized_shared` (see the matching fix comments
        // on each in `vm/src/jit/helpers.rs`), which needs `jit_thread_mut()`
        // to resolve a live `&mut JvmThread` -- exactly the same
        // `JIT_THREAD` TLS this whole `has_dispatch` flag exists to
        // guarantee (see the doc comment above: "otherwise the callee's
        // dispatch helper sees a null thread and silently returns 0").
        // Before this line, a method whose ONLY JIT-relevant content was
        // getstatic/putstatic/new sites -- no invoke, no direct call, no
        // bounds check, nothing else on this list -- compiled with
        // `has_dispatch=false` and was entered through `execute_jit_call`'s
        // fast arm (`vm/src/runtime/interpreter.rs`, `if !compiled.
        // has_dispatch`), which skips `set_jit_thread` entirely. Any of
        // those three helpers then saw `jit_thread_mut() == None` and
        // silently skipped the class-init check altogether (the same
        // "silently returns 0"-shaped failure the `Character.getType`
        // fix above this comment already fixed for `direct_calls`) --
        // confirmed via a minimal repro (`static void touch(boolean w) {
        // if (w) { Init.VALUE = v; } else { dummy++; } }`, no other
        // dispatch-needing construct) whose compiled artifact had
        // `has_dispatch=false`, `static_field_info.len()=3`, and observably
        // wrote the static WITHOUT running `<clinit>` first. `new_info` gets
        // the same treatment for the identical reason on the `jit_new_object`
        // side.
        || !compiler.static_field_info.is_empty()
        || !compiler.new_info.is_empty()
        // `jit_new_object_cp` / `jit_anewarray_object_cp` need `jit_thread_mut()`
        // for even more than the resolved helpers do: class RESOLUTION itself
        // (a possible user `ClassLoader.loadClass`) runs on that thread, not
        // just `<clinit>`. A null thread there would leave the site unable to
        // resolve at all.
        || !compiler.new_deferred_info.is_empty()
        || !compiler.anewarray_deferred_info.is_empty()
        // `jit_ldc_class_cp` needs `jit_thread_mut()` for the same reason the
        // two above do: the resolution it performs may run a user
        // `ClassLoader.loadClass`, and a failure has to publish a pending
        // exception on this thread. `jit_ldc_string_cp` joins it for the
        // second half of that reason only — it loads nothing, but its
        // defensive "the CP entry is no longer readable" arm publishes an
        // `InternalError` through the same channel, and `emit_post_alloc_oom_check`
        // is emitted at its site either way.
        || !compiler.ldc_class_info.is_empty()
        || !compiler.ldc_string_info.is_empty();
    // Snapshot the frame partition and the label BEFORE `compiler.buf` is moved
    // into the artifact (which partially moves `compiler`).
    let frame_layout = compiler.frame_layout();
    // The denominator for any later per-region stale-word census: whether this
    // compile gave the scalar-replacement and LICM-hoist regions an extent at
    // all. A region with no extent cannot receive a word, so a zero count
    // against it is a statement about the optimisation, not about the region.
    frame_layout.record_region_extent_census();
    // DIAGNOSTIC (`CRATONVM_DBG_JIT_SLOT_OVERLAP=1`), here because this is the
    // last point at which the whole compile is still in one piece: every frame
    // slot this body READS and never WRITES. A no-op unless the flag is set.
    compiler.dbg_report_never_stored_slots();
    let method_label = compiler.method_label.clone();
    // Taken before `compiler.buf` moves into the artifact. Registration waits
    // until `cm` exists, so a compile that bails before that registers
    // nothing — and one that bails after is unregistered by
    // `CompiledMethod::drop`, which retires the whole code range.
    let implicit_null_sites = std::mem::take(&mut compiler.implicit_null_sites);
    let mut cm = if needs_heap {
        CompiledMethod::new_with_context(compiler.buf)
    } else {
        CompiledMethod::new(compiler.buf)
    };
    cm.has_dispatch = has_dispatch;
    if !implicit_null_sites.is_empty() {
        let base = cm.entry as usize;
        // The entry MUST be the buffer base, because `CompiledMethod::drop`
        // retires `[entry, entry + buffer.pos())` while this registers at
        // `entry + fault_off`. Today they agree (`entry_offset = 0` above, and
        // the OSR-trampoline purge in that same `Drop` already leans on it).
        // If a future prologue moves the entry, every site below it silently
        // stops being retired -- a stale entry pointing into a reused buffer,
        // which is the exact hazard this design exists to prevent, arriving
        // through the one door nobody would think to check.
        //
        // So it is checked, and a mismatch registers NOTHING: the sites keep
        // their elided checks and the faults they would have caught go to the
        // crash reporter. That is a real loss of NPEs and it is the safe
        // direction -- a crash is diagnosable, a stale recovery is not.
        if base != cm.code_bytes().as_ptr() as usize {
            crate::note_jit_bail_site_at("implicit-null-entry-not-base", 0, 0);
        } else {
            for (fault_off, recover_off) in implicit_null_sites {
                // A full table DECLINES. The site keeps its elided check, and the
                // fault it would have caught then arrives as a crash instead of an
                // NPE — so a decline is a real loss, not a graceful degradation,
                // and that is why `implicit_null::counts` prints it rather than
                // swallowing it.
                let _ = crate::implicit_null::register(base + fault_off, base + recover_off);
            }
        }
    }

    // RBC.5 — record the declaring classes of every getstatic/putstatic
    // site (already resolved into `static_field_info` by the caller) so the
    // interpreter's compiled-entry fast path can ensure-initialize them
    // once per artifact instead of re-resolving the constant pool on every
    // call (see `static_init_classes` on `CompiledMethod`).
    cm.static_init_classes = {
        let mut ids: Vec<u32> = compiler
            .static_field_info
            .iter()
            .map(|&(_, class_id_raw, ..)| class_id_raw)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    // Store OSR metadata for On-Stack Replacement entry.
    //
    // Built and validated by `x64/osr.rs`, which owns the three-coordinate-space
    // reasoning this needs and the fail-closed rule it ends in. `osr_entry_native`
    // is handed over by value: nothing below reads it again.
    osr::publish_entry_metadata(
        &mut cm,
        code,
        code_len,
        orig_code_len,
        &loop_xform,
        compiler.osr_entry_native,
        &compiler.local_assignments,
        &compiler.xmm_assignments,
        &compiler.osr_block_live_in,
        compiler.num_locals,
        compiler.num_reg_locals,
        kernel_reg_homes,
        kernel_reg_homes_osr_requested,
        &method_label,
    );
    cm.osr_frame_size = compiler.frame_size;
    cm.osr_callee_saved_base = compiler.callee_saved_base;
    // HIB-CV-20 OSR caller-corruption fix: hand the trampoline the EXACT
    // callee-saved sets the epilogue restores (GPR + XMM), so it spills the
    // caller's value for every one of them at the matching slot index. The
    // prologue/epilogue spill/restore `alloc_used_regs` / `alloc_used_xmms` (the
    // full allocator-used set, which can include callee-saved regs used for
    // operand-stack temporaries — NOT just locals); the old trampoline only
    // spilled a local_assignments-derived subset, so any non-local callee-saved
    // register was restored from the wrong (or an uninitialised) slot, silently
    // corrupting the OSR caller's live registers after return.
    cm.osr_callee_saved_regs = Some(compiler.alloc_used_regs.clone());
    cm.osr_callee_saved_xmms = Some(compiler.alloc_used_xmms.clone());
    cm.osr_xmm_saved_base = compiler.xmm_saved_base;
    cm.method_label = method_label;
    cm.shadow_savebase_slot_off = compiler.shadow_savebase_slot_off;
    cm.frame_layout = frame_layout;
    cm.osr_heap_local_offset = compiler.heap_local_offset;
    cm.jit_thread_slot_off = compiler.jit_thread_slot_off;
    cm.stack_floor_slot_off = compiler.stack_floor_slot_off;
    cm.osr_frame_record = compiler.helpers.frame_record;

    // T1.1.a — transfer precise oop maps collected during codegen.
    // The GC root walker's `JitEntryGuard::enter_with_compiled` path
    // checks `CompiledMethod::has_precise_oop_maps()` to decide
    // whether to use them for this frame; when empty, it falls back
    // to the conservative stack scan for that frame — always a
    // correct super-set of the precise coverage.
    cm.oop_maps = compiler.oop_maps;
    // deopt-osr Step 1: transfer precise deopt-exit snapshots collected at
    // eligible guards (currently the speculative-BCE loop-header guard). No live
    // path consumes these yet — emit-and-discard until the in-stub trampoline +
    // resume land (real-frame-deopt-x64-backport Steps 2-4) — so this is inert
    // (find_deopt_point has no live caller; the i64::MIN re-run is unchanged).
    cm.deopt_points = compiler.deopt_points;
    cm._deopt_point_boxes = compiler.deopt_boxes;
    // deopt-osr Step 9 follow-up (a): hand the retained epoch guard (baked as the
    // 4th arg into every frame-deopt stub) to the artifact so the VM can stamp it
    // (creation epoch + live-epoch cell) at install. Null on production artifacts
    // (no frame-deopt stub emitted unless `deopt_real_enabled()`).
    cm.deopt_epoch_guard = compiler.deopt_epoch_guard;
    // deopt-osr x64-backport Step 5 — finalize the per-method deopt-resume
    // coverage gate (mirrors `fully_oop_covered` / `can_osr_exit`). A method may
    // resume a real-frame deopt only when:
    //   1. it emitted at least one deopt-exit snapshot (`deopt_points`), and
    //   2. it scalar-replaced NO objects (`scalar_replaced` empty).
    // (2) is load-bearing: this backend records a scalar-replaced slot by its
    // machine provenance (Register/StackSlot), NOT as a `VirtualObject`, so its
    // snapshot cannot be re-materialized — and lock elision over such an object
    // makes mid-method resume unsound (the elided-monitor hazard). Until the x64
    // emitter writes `VirtualObject` deopt slots + an elided-monitor flag, a
    // scalar-replacing method stays on the safe re-run path. Empty/false unless
    // `deopt_real_enabled()` (the snapshot emit site is gated), so production
    // artifacts are unchanged. Consumed at the interpreter deopt sink, which
    // attempts `resume_real_ir_deopt` only when `compiled.can_deopt_resume`.
    //
    // P2.1/P2.2 (cat-2 + FP resume): the snapshot now has a per-slot WIDTH source
    // (`classify_local_kinds`) and emits typed `RegisterLong`/`StackSlotLong`
    // (long), `XmmFloat`/`XmmDouble`/`StackSlotFloat`/`StackSlotDouble` (FP) values
    // that the resume mapper reconstructs as full-width `Value::Long`/`Float`/
    // `Double`; the deopt stub spills XMM0..15 (under the gate) so the XMM-resident
    // FP forms resolve. P2.0's blanket wide-local exclusion is fully lifted. Any
    // slot the classifier can't type (Ambiguous / a register-resident ref /
    // contradiction) emits `Unsupported`, and the mapper re-runs the whole method
    // for it — the per-slot fine-grained safety net behind this coarse gate.
    //
    // Phase B (real-frame-deopt x64 backport): the scalar-replacement exclusion is
    // RELAXED. The snapshot builder now emits `FrameValue::VirtualObject` /
    // `VirtualObjectRef` for a scalar-replaced object live in a local at a deopt
    // point (its fields read from their frame slots, typed by `sr_field_types`),
    // which the VM materializer (`deopt_materialize::materialize_virtual_objects`)
    // rebuilds on resume. So a scalar-replacing method MAY resume — UNLESS it
    // elided a `monitorenter`/`monitorexit` over a scalar object (`has_elided_monitor`):
    // an elided lock leaves no `monitors` trace, so the resume would skip the
    // re-lock (the elided-monitor hazard, deferred to Phase C). `ACC_SYNCHRONIZED`
    // is independently caught by the VM resume sink's `is_synchronized` bail.
    cm.can_deopt_resume = !cm.deopt_points.is_empty() && !compiler.has_elided_monitor;
    // deopt-osr Step 7 — transfer the OSR-exit loop-boundary bci set and set the
    // per-method gate. Both are empty/false unless `deopt_real_enabled()` was on
    // (the emit site is gated), so production artifacts are unchanged. Step 8
    // consults `can_osr_exit` + `osr_exit_points` (under `CRATONVM_DEOPT_REAL`)
    // to route a mid-loop bail through the deopt trampoline.
    // Bcis, consumed by the VM's OSR-exit route, so interpreter-bci space —
    // `filter_map` drops a pc with no provenance instead of publishing it raw.
    // A LIVE path since the bci translation retired the `deopt_real` and
    // `invokedynamic` refusals: every copy of a loop-boundary bytecode records
    // its own exit map, and all of them collapse onto the one original bci, so
    // deduplicate rather than publish the same bci `copies + 1` times.
    cm.osr_exit_points = match &loop_xform {
        Some(x) => {
            let mut seen = FxHashSet::default();
            compiler
                .osr_exit_points
                .iter()
                .filter_map(|&pc| x.bci_at(pc))
                .filter(|bci| seen.insert(*bci))
                .collect()
        }
        None => compiler.osr_exit_points,
    };
    // FIX: mirror `can_deopt_resume`'s elided-monitor exclusion above — an
    // OSR-exit transfer materializes the same kind of reconstructed frame, so
    // a scalar-replaced object held under an elided `synchronized` block is
    // the same unsound-resume hazard here as it is for `can_deopt_resume`.
    cm.can_osr_exit = !cm.osr_exit_points.is_empty() && !compiler.has_elided_monitor;
    // jit-invokedynamic-groovy-regression fix: a compiled 0xba site is an
    // UNCONDITIONAL trap (the instruction is never JIT-executed), so any
    // execution of this artifact that reaches it deopts. Publishing this
    // artifact's entry where MACHINE CODE calls it directly (a baked
    // JIT→JIT direct call, a MIC/PIC inline-cache entry) would let the
    // sentinel + stashed frame bail through a compiled CALLER's epilogue,
    // where no consumer can resume the callee precisely (the caller's
    // continuation is already lost). The publication gates (see
    // `callee_compiler` in vm/src/runtime/interpreter.rs and the MIC/PIC
    // install sites in vm/src/jit/helpers.rs) consult this flag so every
    // call to such a method stays on a dispatch helper, whose
    // `try_resume_trapped_callee` resolves the trap precisely in place.
    // Only sites that actually lower to a trap count. A fully bridged method
    // — every indy is a `StringConcatFactory` or `LambdaMetafactory` site the
    // bridge serves — carries no trap, so it must not be forced onto the
    // dispatch-helper path for its callers. This predicate is the SAME one
    // `bytecode_walk`'s 0xba arm uses to decide whether to emit the bridge
    // call; the two must not drift, or an artifact would advertise a trap it
    // does not have (or, far worse, hide one it does).
    cm.has_indy_trap = {
        let bridge_entry = crate::INDY_BRIDGE_FN.load(std::sync::atomic::Ordering::Relaxed);
        compiler
            .indy_info
            .iter()
            .any(|(_pc, _arg_slots, ret_type, _tags, bridge_site)| {
                !(*bridge_site != 0 && bridge_entry != 0 && *ret_type != b'V')
            })
    };
    // Stage 3 — the frame offset where this method stores the active
    // safepoint's bytecode PC (0 when the precise gate was off at compile).
    cm.sp_id_slot_off = compiler.sp_id_slot_off;
    // The identity half of the frame record. The prologue this compile emitted
    // publishes `compiler.compile_id` into the compile-id mirror on entry, and
    // `bind_compile_id` at publication binds whatever `CompiledMethod::compile_id`
    // holds — so leaving it 0 here bound NOTHING (`bind_compile_id` early-returns
    // on 0) while every frame of this method went on publishing an id no lookup
    // could resolve.
    //
    // The consequence was silent and only visible from Java: with the id
    // unresolvable, `conservative_roots::innermost_frame_method` fell through to
    // the boundary method, so the INNERMOST compiled frame of every fast-tier
    // method was reported under its CALLER's name. `SWCross` shows it directly —
    // a `helper()` frame read as a second `recurse` — and it is why Log4j2's
    // caller lookup answered with the enclosing class. The optimizing backend
    // never had the bug: `ir_lower.rs` assigns `cm.compile_id` at its own
    // finalize, which is the line this mirrors.
    // Handed off, not copied: a compile that bails before this line drops
    // the reservation, which releases the id.
    cm.compile_id = compiler.compile_id.hand_off();
    // Stage A.2 (precise oop maps, B-K fix) — a method is "fully precisely
    // covered" only when EVERY GC-capable safepoint that flushed its
    // register-locals (`safepoint_pcs`) also recorded a precise oop map
    // (`mapped_safepoint_pcs`), the precise gate is on (so the sp-id slot
    // exists and the per-safepoint id is stored), and there is no construct the
    // current mapping cannot describe (inlined-callee safepoints). OSR entry
    // used to be a coverage breaker because it bypassed the prologue's shadow
    // and exact-RBP setup; the OSR trampoline now mirrors both before jumping to
    // the loop body, so OSR artifacts use the same completeness predicate.
    // Stage B consults this to decide whether the GC may skip the conservative
    // backstop for this frame and treat its precise oops as movable. It is a
    // NECESSARY codegen precondition; the runtime `CRATONVM_DBG_VERIFY_OOP_MAPS`
    // oracle (Stage G0) is the SUFFICIENT proof that must gate the actual
    // backstop suppression before the moving path relies on it. Always `false`
    // on the default path (`sp_id_slot_off == 0`), so it is inert until the gate
    // is on AND Stage B lands.
    //
    // THE INLINE TERM IS RETIRED, AND THE MASK IT HID BEHIND IS CLOSED
    // (2026-09-02). `compiler.inline_sites.is_empty()` was the "no construct the
    // current mapping cannot describe" clause, written when a splice was exactly
    // that: its callee's locals lived in the caller's spill area and nothing
    // named them. Stage 3b names them now -- each live spliced-callee local goes
    // into this safepoint's own `frame_slot_offsets`, and a scope whose dataflow
    // cannot classify the callee pc fails the safepoint closed
    // (`INLINE_LOCAL_UNMAPPABLE`). The blanket term was refusing methods the
    // sharper per-safepoint machinery had already cleared: `RMapGcStress.key`
    // reads `shadow=true`, every one of the eight `causes` zero, `unmapped_pcs=[]`
    // -- and `frameslot=false` for no reason but this clause.
    //
    // Retiring it alone would NOT have been safe, which is why the count below
    // lands with it. `safepoint_pcs.is_subset(&mapped_safepoint_pcs)` is keyed by
    // BYTECODE PC, and a splice emits one safepoint per `invoke*` in the callee
    // under ONE enclosing bci -- so a complete map at that bci inserts the pc and
    // the subset test then reads TRUE with an incomplete map beside it. The
    // blanket term was incidentally covering that hole for exactly the shape that
    // opens it. `incomplete_oop_maps` counts safepoints, not pcs, and cannot be
    // masked; it is strictly stronger than the subset test for this purpose, and
    // the subset test is kept because it also catches a safepoint that pushed no
    // map at all.
    //
    // `CRATONVM_JIT_INLINE_OOP_COVERAGE=0` restores the previous predicate
    // verbatim, so the pair is one A/B in one binary.
    cm.fully_oop_covered = compiler.precise_maps
        && compiler.sp_id_slot_off != 0
        && (if inline_oop_coverage_enabled() {
            compiler.incomplete_oop_maps == 0
        } else {
            compiler.inline_sites.is_empty()
        })
        && compiler
            .safepoint_pcs
            .is_subset(&compiler.mapped_safepoint_pcs);
    // The SHADOW aggregate, and a different question from the four terms above
    // — see `CompiledMethod::fully_shadow_covered`. `cm.oop_maps` was assigned
    // above, so this is the complete set this compilation pushed.
    cm.fully_shadow_covered =
        !cm.oop_maps.is_empty() && cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
    // `CRATONVM_DBG_OOPCOV=1` — WHICH of the four terms said no, per method.
    //
    // `moving_young_osr_method_needs_fallback` reports the aggregate as one
    // `map_coverage` counter, and that counter is what
    // `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` was left
    // holding: it names the field, not the term, and not the method. The four
    // terms fail for completely different reasons (a gate that is off, a slot
    // the prologue did not reserve, an inlined callee, a safepoint that
    // flushed without recording), so an aggregate cannot be acted on.
    //
    // Only the unmapped PCs are listed, capped: on a large method
    // `safepoint_pcs` can hold hundreds of entries and the difference is the
    // whole content of the report.
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OOPCOV")
        && !(cm.fully_oop_covered && cm.fully_shadow_covered)
    {
        let mut missing: Vec<u32> = compiler
            .safepoint_pcs
            .difference(&compiler.mapped_safepoint_pcs)
            .copied()
            .collect();
        missing.sort_unstable();
        missing.truncate(16);
        // The safepoints whose SHADOW claim is false, which is the aggregate
        // the OSR fallback reads. Listed by `bytecode_pc` so it lines up with
        // `unmapped_pcs` — the two sets are different questions and, on the
        // direct-call shape, deliberately disjoint.
        let mut shadow_missing: Vec<u32> = cm
            .oop_maps
            .iter()
            .filter(|m| !m.moving_young_coverage_complete)
            .map(|m| m.bytecode_pc)
            .collect();
        shadow_missing.sort_unstable();
        shadow_missing.dedup();
        shadow_missing.truncate(16);
        let scauses = crate::x64::safepoint::shadow_incomplete_cause::snapshot();
        let causes = crate::x64::safepoint::map_incomplete_cause::snapshot();
        // A census that prints fewer causes than it has is a census that can
        // report "no cause" for a real one. Adding a variant without adding a
        // column is now a compile error rather than a silent column.
        const _: () = assert!(
            crate::x64::safepoint::map_incomplete_cause::COUNT == 9,
            "map_incomplete_cause gained a variant: add a column to the              frameslot-detail line below, then bump this"
        );
        eprintln!(
            "[oopcov] uncovered method={} frameslot={} shadow={} \
             shadow_missing_pcs={shadow_missing:?} \
             scauses(gate={} desync={} marks={} scratch={} locals64={} dataflow={} nopush={} inline_scope={}) ",
            compiler.method_key,
            cm.fully_oop_covered,
            cm.fully_shadow_covered,
            scauses[0],
            scauses[1],
            scauses[2],
            scauses[3],
            scauses[4],
            scauses[5],
            scauses[6],
            scauses[7],
        );
        eprintln!(
            "[oopcov]   frameslot-detail method={} precise_maps={} sp_id_slot_off={} inline_sites={} \
             safepoints={} mapped={} unmapped_pcs={:?} \
             causes(marks_inexact={} oop_in_reg={} stack_deep={} local_deep={} staged_deep={}              staged_unmappable={} inline_local_unmappable={} local_mask_unreached={}              local_mask_unsupported={})",
            compiler.method_key,
            compiler.precise_maps,
            compiler.sp_id_slot_off,
            compiler.inline_sites.len(),
            compiler.safepoint_pcs.len(),
            compiler.mapped_safepoint_pcs.len(),
            missing,
            causes[0],
            causes[1],
            causes[2],
            causes[3],
            causes[4],
            causes[5],
            // The SEVENTH cause. `map_incomplete_cause::snapshot()` has returned
            // `[usize; 7]` since `INLINE_LOCAL_UNMAPPABLE` was added, and this
            // line printed six of them -- so a run whose only unnameable
            // references were inline-scope locals showed `causes(... all zero)`
            // and read as "no cause", which is the one reading a cause census
            // must never produce. The 2026-08-30 diagnosis that concluded "One
            // cause, `staged_unmappable`" was made from this line.
            causes[6],
            causes[7],
            // The NINTH cause. A method whose local count exceeds what
            // `compute_local_oop_masks` supports got no mask at all, so Stage 2
            // contributed nothing and bumped nothing -- and the map shipped
            // claiming coverage of locals it had not named. Added as a column in
            // the same commit as the cause, which is what the `COUNT` assertion
            // above exists to force.
            causes[8],
        );
    }
    // Shadow-stack — frame offsets + thread-struct offset, so the OSR trampoline
    // can replicate the prologue's shadow setup (cache the thread ptr + snapshot
    // the `top` watermark) for OSR-entered frames. All 0 when shadow-stack
    // support was off at compile.
    cm.shadow_thread_slot_off = compiler.shadow_thread_slot_off;
    cm.shadow_savetop_slot_off = compiler.shadow_savetop_slot_off;
    cm.shadow_off_in_thread = compiler.shadow_off_in_thread;

    // Task #60 — attach unroll-cloned MIC/PIC slots to the
    // CompiledMethod so they outlive the compiled code. The imm64
    // baked into duplicated `MOV R10, imm64` instructions is a raw
    // pointer to one of these boxes; without keeping them alive on the
    // CompiledMethod, the first GC of the Box would invalidate the
    // pointer and the next invokevirtual on an unrolled copy would
    // dereference freed memory.
    //
    // The caller-supplied slots (from `lib.rs::try_compile`) remain
    // owned by the caller and are attached to `_jit_mic_slots` /
    // `_jit_pic_slots` separately on the lib.rs side. This `extend`
    // is purely additive — both vectors retain their previous
    // contents.
    cm._jit_mic_slots.extend(compiler.cloned_mic_slots);
    cm._jit_pic_slots.extend(compiler.cloned_pic_slots);
    // The local-handler stubs baked each of these addresses as an immediate.
    // Moving them onto the artifact is what keeps those pointers valid for
    // exactly as long as the machine code that reads them — and what frees them
    // with it on tier-up or invalidation. Empty on every compile that armed no
    // local handlers.
    cm._jit_local_handler_sites = compiler.local_handler_sites;

    // Close the recording session and hand the finished map to the artifact,
    // beside the other compile-local state being published onto it above.
    //
    // `code_len()` is load-bearing, not decorative: `InlineFrameMap::from_rows`
    // drops every row at an offset past the artifact's final code length, which
    // is how a row recorded into a stretch the emitter later rewound is
    // discarded instead of published against machine code that is no longer
    // there. Read into a local first so the immutable borrow of `cm` is over
    // before the field assignment, rather than relying on evaluation order.
    //
    // `_inline_frame_session`'s `Drop` still runs on the way out of this
    // function; that second close is a no-op (see `InlineFrameSession`).
    let inline_frame_code_len = cm.code_len();
    cm.inline_frame_map = crate::x64::finish_inline_frame_recording(inline_frame_code_len);
    // No `code_len` screen for the trap table, and it needs none: its keys are
    // monotonic ids rather than code offsets, so a row a rewind orphaned is
    // simply unreachable -- no surviving trampoline carries its key. See
    // `x64::inlining::record_npe_trap_site`.
    cm.npe_trap_map = crate::x64::finish_npe_trap_recording();

    Some(cm)
}

/// Estimate the maximum operand stack depth for the method.
/// Simple conservative estimate: count push-like opcodes.
pub(super) fn estimate_max_stack(code: &[u8], code_len: usize) -> usize {
    let mut max_depth = 0usize;
    let mut depth = 0usize;
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // Push operations (aconst_null, load const/local/ref, bipush, sipush → +1)
            0x01..=0x14 | 0x15..=0x19 | 0x1a..=0x2d => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Pop operations (binary ops pop 2, push 1 → net -1)
            0x60..=0x71 | 0x78..=0x83 | 0x94..=0x98 => {
                depth = depth.saturating_sub(1);
            }
            // Store ops pop 1 (i/l/f/d/astore, i/l/f/d/astore_N)
            0x36..=0x4e => {
                depth = depth.saturating_sub(1);
            }
            // Array load: pop 2 (array, index), push 1 → net -1
            0x2e..=0x35 => {
                depth = depth.saturating_sub(1);
            }
            // Array store: pop 3 (array, index, value) → net -3
            0x4f..=0x56 => {
                depth = depth.saturating_sub(3);
            }
            // getstatic: push 1 (value) → net +1
            0xb2 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // putstatic: pop 1 (value) → net -1
            0xb3 => {
                depth = depth.saturating_sub(1);
            }
            // getfield: pop 1 (objectref), push 1 (value) → net 0
            0xb4 => {}
            // putfield: pop 2 (objectref, value) → net -2
            0xb5 => {
                depth = depth.saturating_sub(2);
            }
            // newarray: pop 1 (count), push 1 (ref) → net 0
            0xbc => {}
            // new: push 1 object ref (no pop) → +1
            0xbb => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // anewarray: pop 1 (count), push 1 (ref) → net 0
            0xbd => {}
            // arraylength: pop 1 (ref), push 1 (int) → net 0
            0xbe => {}
            // Return pops 1 (ireturn, lreturn, freturn, dreturn, areturn)
            0xac..=0xb0 => {
                depth = 0;
            }
            // void return
            0xb1 => {
                depth = 0;
            }
            // Unary ops (neg, conversions): pop 1, push 1 → net 0
            0x74..=0x77 | 0x85..=0x93 => {}
            // Branch pops (ifXX pop 1, if_icmpXX pop 2, if_acmpXX pop 2)
            0x99..=0x9e => {
                depth = depth.saturating_sub(1);
            }
            0x9f..=0xa6 => {
                depth = depth.saturating_sub(2);
            }
            // ifnull/ifnonnull pop 1
            0xc6 | 0xc7 => {
                depth = depth.saturating_sub(1);
            }
            // checkcast: pop 1, push 1 → net 0
            0xc0 => {}
            // instanceof: pop 1, push 1 → net 0
            0xc1 => {}
            // Pop
            0x57 => {
                depth = depth.saturating_sub(1);
            }
            // Pop2
            0x58 => {
                depth = depth.saturating_sub(2);
            }
            // Dup: +1
            0x59 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // dup_x1 / dup_x2 add one copy of the top operand.
            0x5a | 0x5b => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // dup2* can add two category-1 slots; category-2 values are still
            // one slot in this x64 operand-stack model.
            0x5c..=0x5e => {
                depth += 2;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Swap: 0
            0x5f => {}
            // iinc: 0
            0x84 => {}
            // goto: 0
            0xa7 => {
                depth = 0;
            }
            // jsr pushes a returnAddress in legacy bytecode.
            0xa8 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // invokestatic/invokevirtual/invokespecial/invokeinterface: conservatively
            // assume they push 1 result (pops are hard to estimate without descriptors)
            0xb6..=0xb9 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // multianewarray: pops ndims, pushes 1 → net -(ndims-1)
            0xc5 => {
                let ndims = code.get(pc + 3).copied().unwrap_or(2) as usize; // Cast: address arithmetic
                depth = depth.saturating_sub(ndims.saturating_sub(1));
            }
            _ => {}
        }
        // Advance PC via the canonical length table. The ad-hoc copy this
        // replaces was missing `ldc` (0x12) and treated tableswitch/
        // lookupswitch as 1-byte, so the walk stepped through operand bytes
        // (incl. switch pad/offset tables) as phantom opcodes; a phantom
        // return zeroed `depth` and could UNDER-estimate the frame's operand
        // stack (the CM-FASTMATH length-table desync family).
        let len = bytecode_len_at(code, pc);
        if len == 0 {
            break;
        }
        pc += len;
    }
    // Add safety margin (conservative for invoke stack effects not tracked above)
    max_depth + 4
}
