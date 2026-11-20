#[allow(unused_imports)]
use super::super::*;


/// A compiled native-code method.
pub struct CompiledMethod {
    /// The executable buffer holding the machine code.
    pub(crate) _buffer: ExecutableBuffer,
    /// Entry point — pointer to the start of the compiled code.
    ///
    /// ALWAYS the buffer's first byte (`from_finalized_buffer`), and nothing
    /// may make it otherwise without revisiting
    /// [`pin_jit_code_range_owner_checked`]: its freed-buffer arm answers
    /// `Dead` only for a freed buffer's BASE, so an entry at an interior
    /// offset of a freed body would read `NotJit` and be CALLED (round 14
    /// wave 3, lane fixup, RV3 item 4). [`Self::entry_ptr`] asserts it in
    /// debug builds.
    pub(crate) entry: *const u8,
    /// Whether the method needs a SharedVm pointer as hidden first argument.
    pub(crate) needs_context: bool,
    /// JIT r20 wave 11 (lane pAlloc; W16L2-3 / W17L1-8,
    /// `CRATONVM_JIT_ENTRY_VALIDATED_ONCE`): the entry that passed
    /// [`validate_code_ptr`] when this artifact was built (x86-64, switch on),
    /// else null. [`Self::try_call`] / [`Self::try_call_with_context`] skip
    /// the per-call validation for exactly that pointer (a `debug_assert`
    /// keeps it in debug builds); any other value -- an entry overwritten
    /// since -- takes the full check. See [`Self::validate_entry_once`].
    pub(crate) entry_validated_once: *const u8,
    /// Owned JIT metadata strings. JIT code references these via raw pointers;
    /// they are freed when this `CompiledMethod` is dropped.
    pub _jit_strings: Vec<Box<str>>,
    /// Owned `JitInvokeInfo` structs. JIT code references these via raw pointers;
    /// they are freed when this `CompiledMethod` is dropped.
    pub _jit_invoke_infos: Vec<Box<JitInvokeInfo>>,
    /// Owned [`VarHandleJudgedSite`] records (round 15 wave 6, lane vh8): the
    /// judged thin `VarHandle` binds bake each one's address as their first
    /// argument word, and its `info` is a dispatch site key like those of
    /// `_jit_invoke_infos`. Freed when this `CompiledMethod` is dropped.
    pub _jit_vh_judged_sites: Vec<Box<VarHandleJudgedSite>>,
    /// The per-method entry counter generated code bumps, when
    /// `CRATONVM_JIT_ENTRY_COUNTER` is on (the default since round 9 wave 3).
    /// `None` under the kill switch `CRATONVM_JIT_ENTRY_COUNTER=0` (or a
    /// thread override pinning it off) — the prologue then emits nothing and
    /// this stays unset, so such a run is byte-identical to one from before
    /// the counter existed.
    ///
    /// Boxed and owned by the `CompiledMethod` for the same reason
    /// `_jit_invoke_infos` is: generated code holds its address as a
    /// `mov r10, imm64`, so the allocation must not move for the life of the
    /// body and must outlive every frame that could still be executing it.
    /// The leading underscore follows this struct's convention for fields that
    /// exist to keep something alive at a stable address rather than to be read
    /// on a hot path.
    pub _jit_entry_counter: Option<Box<entry_counter::JitEntryCounter>>,
    /// Live branch cells whose stable addresses this artifact embeds.
    pub(crate) _jit_c1_branch_counters: Option<Arc<profile::BranchCounters>>,
    /// Owned [`JitLocalHandlerSite`] records, one per throwing bci this method
    /// can catch itself. The local-handler stubs bake a raw pointer to each;
    /// they are freed when this `CompiledMethod` is dropped.
    pub _jit_local_handler_sites: Vec<Box<JitLocalHandlerSite>>,
    /// Owned monomorphic inline cache slots. JIT code references these via raw
    /// pointers; they are freed when this `CompiledMethod` is dropped.
    pub _jit_mic_slots: Vec<Box<JitMICSlot>>,
    /// Owned polymorphic inline cache slots. JIT code references these via
    /// raw pointers (embedded as imm64 in the inline 4-way cascade emitted
    /// by `Compiler::compile_op_invokevirtual`); they are freed when this
    /// `CompiledMethod` is dropped.
    ///
    /// HIGH-7 — populated eagerly at first compile for every
    /// invokevirtual / invokeinterface bci. Slots start empty (all 4
    /// class ids at `JitPICSlot::EMPTY_WAY_CLASS_ID`, which no receiver
    /// matches — not `0`, which `ClassId(0)` would), so the inline cascade falls
    /// straight through to the slow-path helper on cold sites; once the
    /// helper has populated a slot, subsequent dispatches take the
    /// inline fast path.
    pub _jit_pic_slots: Vec<Box<JitPICSlot>>,
    /// Owned [`JitCycleEdgeCell`]s: JIT code bakes their addresses (the
    /// guarded closing edge of a compile-time recursion cycle), so they live
    /// exactly as long as this body. [`JitCache`] holds only `Weak`s.
    pub _jit_cycle_cells: Vec<Arc<JitCycleEdgeCell>>,
    /// Owned seen-class cells of the interface constant handle guards'
    /// `instanceof`s (JIT round 20, MHIFACE19-1; `Op::InstanceOf::seen_cell`).
    /// JIT code bakes their addresses, so they live exactly as long as this
    /// body.
    pub _jit_typecheck_seen_cells: Vec<Box<TypecheckSeenCell>>,
    /// Native entries of compiled callees referenced by baked direct calls.
    /// Filled by the compile driver before publication.
    pub _direct_callee_entries: Vec<usize>,
    /// `(entry, artifact_id)` for every raw direct CALL baked into this body:
    /// the address, and WHICH artifact owned it when the address was baked.
    ///
    /// Without the second half, `prepare_for_publication` can only ask whether
    /// something live owns the address now. Measured on
    /// `TestWebdavPropertyStore` 2026-08-28, one run's log, in order:
    ///
    /// ```text
    /// full-compile SQLChar.<init>()V     entry=E len=1362
    /// [jit-code-free] base=E published=true authorised=false
    /// full-compile SQLInteger.<init>()V  entry=E len=657      <- E recycled
    /// full-compile DataValueFactoryImpl.getNullDVDWithUCS_BASICcollation
    /// [punned-store-jit] ... executing at E+0x224
    /// ```
    ///
    /// The caller baked `CALL E` for `SQLChar.<init>`; by publication E was
    /// `SQLInteger.<init>`, whose body opens `putfield isnull:Z` -- field
    /// index 1, `Int(1)`. `SQLChar` slot 1 is `rawData`, declared `[C`, so
    /// Derby then read that field as the pointer `1`.
    pub _direct_callee_expected: Vec<(usize, u64)>,
    /// Strong ownership matching `_direct_callee_entries`. A loaded caller can
    /// therefore never outlive executable code reached by one of its baked
    /// calls, even after the callee is invalidated and removed from the cache.
    ///
    /// `RetainedCode`, not bare `Arc`s: a caller that is never installed (it
    /// lost a publication race, or `put` refused it) is dropped at the end of
    /// `JitCache::put`, and one of its roots can be the LAST owner of a callee
    /// the cache has already withdrawn. As a bare `Arc` that drop unmapped a
    /// published body outside the retirement queue
    /// (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
    pub(crate) _direct_callee_roots: Vec<RetainedCode>,
    /// OSR metadata: bytecode PC → native offset mapping.
    pub osr_pc_to_native: Option<Vec<i32>>,
    /// OSR metadata: number of locals in the compiled frame.
    pub osr_num_locals: usize,
    /// OSR metadata: number of register-mapped locals.
    pub osr_num_reg_locals: usize,
    /// OSR metadata: per-local GPR register assignments from graph-coloring allocator.
    pub osr_local_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: per-bytecode-PC "dead local" mask. `osr_dead_mask[pc]` bit
    /// `i` set means local `i` is dead at that OSR entry PC AND its home
    /// register is also some *live* local's home there (graph-colouring
    /// coalescing reused the register once local `i`'s range ended). The OSR
    /// trampoline skips loading exactly those locals, leaving each shared
    /// register to its live owner — see [`osr_dead_local_entry_allowed`] for
    /// why that is sufficient. Indexed like `osr_pc_to_native`.
    pub osr_dead_mask: Option<Vec<u64>>,
    /// OSR metadata: per-local XMM register assignments for float/double locals.
    pub osr_xmm_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: frame size (for SUB RSP).
    pub osr_frame_size: i32,
    /// OSR metadata: callee-saved register save area offset.
    pub osr_callee_saved_base: i32,
    /// OSR metadata: the EXACT callee-saved GPR set the method's epilogue
    /// restores (`alloc_used_regs`), in slot order. The OSR trampoline MUST
    /// spill the caller's value for every register in this set, at the same
    /// `callee_saved_base + i*8` slot the epilogue reads register `i` from —
    /// otherwise (HIB-CV-20) a callee-saved register the allocator used for a
    /// NON-local temporary (e.g. an operand-stack value held across a call) is
    /// restored from the wrong slot, silently corrupting the OSR caller's live
    /// registers. `None` falls back to the (unsound for that case) local-derived
    /// spill set.
    pub osr_callee_saved_regs: Option<Vec<u8>>,
    /// gen r5w6/oomjit10: the optimizing tier's twin of
    /// [`Self::osr_callee_saved_regs`] for the collector: `(register, frame
    /// offset)` of every callee-saved GPR this body's prologue saves
    /// (`ir_lower::Lowerer::saved_gpr_regs`), `Some(vec![])` for a body that
    /// saves none. `None` on every body the optimizing tier did not publish
    /// (the single-pass tier answers through `osr_callee_saved_regs`).
    ///
    /// Read by the band scan (`conservative_roots::caller_register_claim`)
    /// with one fact about the tier: an optimizing-tier body never holds a
    /// reference in one of these registers (`regalloc::xmm_roles::
    /// IR_GP_LINEAR_SCAN` refuses `IrType::Ref`), so a callee's image of a
    /// register this body saved is not a live reference OF THIS BODY; and a
    /// register this body does not save it never writes, so the callee's
    /// image of it is this body's caller's value.
    pub ir_saved_gpr_slots: Option<Vec<(u8, i32)>>,
    /// OSR metadata: the callee-saved XMM set the epilogue restores
    /// (`alloc_used_xmms`), in slot order, paired with `osr_xmm_saved_base`. The
    /// trampoline spills the caller's XMM values to these slots so the epilogue
    /// restores them unchanged (the old trampoline omitted XMM spills entirely,
    /// corrupting the caller's callee-saved XMM regs on Windows). `None`/empty →
    /// no callee-saved XMM in use.
    pub osr_callee_saved_xmms: Option<Vec<u8>>,
    /// OSR metadata: XMM callee-saved save-area offset (mirrors the prologue's
    /// `xmm_saved_base`). Paired with `osr_callee_saved_xmms`.
    pub osr_xmm_saved_base: i32,
    /// Where each storage class lives in this method's frame. See
    /// [`FrameLayout`]; consumed by the moving-young frame verifier.
    pub frame_layout: FrameLayout,
    /// `Class.method` label, for diagnostics that have only a frame to go on.
    pub method_label: String,
    /// [`Self::method_label`] as a shared `Arc<str>`, minted on first use by
    /// [`Self::method_label_shared`]. A stack walk copies the label of every
    /// compiled activation it reports, once per captured throwable; sharing it
    /// makes that copy a reference-count bump instead of an allocation.
    pub(crate) method_label_arc: std::sync::OnceLock<std::sync::Arc<str>>,
    /// JIT round 16 wave 15 (lane throwfill): the distinct
    /// [`OopMapEntry::bytecode_pc`] values of [`Self::oop_maps`], sorted, with
    /// the map count they were taken from; built on first use by
    /// [`Self::has_oop_map_for_safepoint_id`].
    pub(crate) safepoint_id_index: std::sync::OnceLock<(usize, Box<[u32]>)>,
    /// Frame offset of the shadow-stack per-push saved-base slot (the third
    /// reserved tail slot). Exposed alongside `shadow_savetop_slot_off` so a
    /// frame walker can recognise all three shadow bookkeeping slots.
    pub shadow_savebase_slot_off: i32,
    /// The IR tier's FRAME BLOCK: this method publishes its reference homes as
    /// indirect shadow-stack entries, once per activation, and the saved-base
    /// slot then holds the block's base (`0` before the first safepoint, bit 0
    /// set if the push overflowed). While a block is published the collector
    /// rewrites those homes THROUGH the shadow stack, and the frame-walk remap
    /// leaves every word an indirect entry names to it (`ShadowStack::
    /// indirect_slots`) — rewriting a word twice takes an already-moved
    /// address as a fresh key. Descriptive: nothing in the collector keys on
    /// it. See `gc::shadow_stack` (`Indirect entries`).
    pub shadow_frame_block: bool,
    /// OSR metadata: offset of VM context pointer in frame.
    pub osr_heap_local_offset: i32,
    /// OSR metadata: frame offset of the inline-TLAB cached `JvmThread*` slot.
    /// Normal entry initializes this in the prologue; OSR entry initializes it
    /// in the trampoline because it jumps past that prologue. 0 when unused.
    pub jit_thread_slot_off: i32,
    /// Frame slot of the prologue-cached native-stack floor for the inline
    /// self-call check. OSR trampolines initialise it to `usize::MAX`
    /// (`RSP > MAX` is unsatisfiable) so OSR-entered frames always take the
    /// out-of-line guard helper. `0` = not reserved.
    pub stack_floor_slot_off: i32,
    /// OSR metadata: optional `jit_frame_record` helper pointer. Normal method
    /// entry records exact RBP from the JIT prologue; OSR bypasses that
    /// prologue, so the trampoline records its own RBP after `mov rbp, rsp`.
    /// 0 when precise maps are disabled or the helper table is absent.
    pub osr_frame_record: usize,
    /// Shadow-stack: frame offset (`[rbp - off]`) of the cached thread-pointer
    /// slot. A normal entry sets it in the prologue. An OSR entry bypasses that
    /// prologue, so the OSR trampoline stores the real `*mut JvmThread` passed
    /// through `osr_enter` and snapshots the shadow watermark before jumping to
    /// the loop body. 0 when shadow disabled.
    pub shadow_thread_slot_off: i32,
    /// Shadow-stack: frame offset (`[rbp - off]`) of the saved `top` watermark
    /// slot. The OSR trampoline snapshots `thread.shadow_stack.top` here at entry
    /// (mirroring the prologue) so the method epilogue restores it on return,
    /// unwinding any unbalanced push this OSR-entered frame made. 0 when disabled.
    pub shadow_savetop_slot_off: i32,
    /// Shadow-stack: byte offset of the `ShadowStack` within `JvmThread` (its
    /// `top` field is at offset 0, the JIT contract). Used by the OSR trampoline
    /// to load the entry watermark `[thread + off]`. 0 when shadow disabled.
    pub shadow_off_in_thread: i32,
    /// Whether the compiled code uses invoke dispatch (needs set_jit_thread + catch_unwind).
    /// Methods with only direct calls can skip this overhead.
    pub has_dispatch: bool,
    /// Which backend produced this code: `true` for the optimizing IR pipeline,
    /// `false` for the single-pass `x64::compile` backend (incl. any method that
    /// began on the IR path but BAILED to single-pass). Introspection only — used
    /// by tests to assert backend routing (e.g. that a self-recursive long/FP
    /// method stays on single-pass, the fib44-regression guard). Not read by codegen;
    /// read by [`JitCache::put`]'s keep-live-optimizing-body check (R12-4).
    pub used_ir_backend: bool,
    /// Methods that were inlined into this compiled method, and the receiver
    /// types its guarded sites speculated on.
    /// Each entry is (class_name, method_name, descriptor).
    /// Used by invalidation: if the inlined method's class changes, this code must be evicted.
    ///
    /// Populated from [`InlinePlan::invalidation_triples`], so a SPECULATIVE
    /// site contributes both its callee and the class it guarded on — see
    /// [`InlineDependency`] for why the second entry is a retirement
    /// obligation rather than a correctness one.
    pub inlined_methods: Vec<(String, String, String)>,
    /// Per-compilation inlining decision totals. Introspection only — never
    /// read by codegen. Present on every artifact (default-empty when nothing
    /// was inlined) so `metrics::CompileRecorder::installed` can harvest it
    /// from the artifact rather than needing a new thread through
    /// `try_compile_inner`'s ~40 return paths.
    pub inline_tally: InlineDecisionTally,
    /// PC -> inline-chain map: at each call emitted from inside a spliced
    /// body, the callees this artifact inlined at that point, INNERMOST
    /// FIRST. Produced by `x64::inlining` during codegen and attached by
    /// `x64::driver::compile_with_param_slots`; read by
    /// `vm/src/jit/conservative_roots.rs` to give a warmed-up stack trace the
    /// frames an inlined callee otherwise contributes none of (defect 2 of
    /// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902`).
    ///
    /// Introspection and diagnosis ONLY. Never read by codegen, never by the
    /// GC root walk, never by deopt. A level here names a method and a bci and
    /// carries no locals, so it cannot be mistaken for a resume state -- which
    /// is the whole reason it is a separate side table rather than a
    /// correction to `DeoptimizationPoint::frame_state.caller`. That chain
    /// looks like the same fact and is not: every level stamps the COMPILING
    /// method's key because `build_frame_state_at` has no other identity to
    /// stamp, and a nested scope's locals are captured out of the compiling
    /// method's frame, so the scope is malformed as a whole. Correcting only
    /// the NAME there would flip a sound reject into a resume with garbage
    /// locals; see `.agent-requests/A18-jit-lib.txt`.
    ///
    /// COST on a method that splices nothing -- the overwhelming majority --
    /// is exactly `InlineFrameMap::default()`: two empty `Vec`s, 48 bytes
    /// inline in the artifact, zero heap and zero allocation. Such a method
    /// never enters `x64::inlining` at all, so it records nothing either.
    /// `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` makes even a splicing method
    /// retain nothing, so the retained metadata and the extra trace frames
    /// are A/B-able together inside one binary.
    pub inline_frame_map: crate::x64::InlineFrameMap,
    /// Where each of this artifact's INLINE NULL CHECKS would raise: the
    /// trapping bci in this method's own code, plus the spliced callees around
    /// it, keyed by the id its cold trampoline passes to
    /// `jit_npe_with_action`.
    ///
    /// The one program point a compiled frame reaches with no safepoint id to
    /// its name, and therefore the one an `ActiveCompiledFrame` could not put a
    /// line on. See `x64::NpeTrapSite`. Empty -- and allocation-free -- for
    /// every method with no inline null check and for
    /// `CRATONVM_JIT_NO_NPE_TRAP_LINES=1`.
    ///
    /// Introspection and diagnosis ONLY: read exactly once, by
    /// `vm/src/jit/helpers.rs` while it snapshots the frames for an implicit
    /// NPE. Never by codegen, the GC root walk or deopt.
    pub npe_trap_map: crate::x64::NpeTrapMap,
    /// Optimizing-tier safepoint id -> the BYTECODE INDEX that safepoint sits
    /// at, ascending by id. Empty for every single-pass artifact.
    ///
    /// `OopMapEntry::bytecode_pc` is not a bci on this backend: `ir_lower`
    /// stores a monotonic safepoint counter starting at 1 there, because an IR
    /// safepoint is a NODE and several nodes can share one bci. Those counters
    /// are small integers indistinguishable from plausible bcis, and the
    /// artifact's own table records them -- so a stack walk that trusted the
    /// slot would print a confidently WRONG line rather than none, which is why
    /// `conservative_roots::activation_bci` refused an `used_ir_backend`
    /// artifact outright and every optimizing-tier frame reported
    /// `(Unknown Source)`. This is the translation that refusal was waiting
    /// for: one `(id, bci)` pair per emitted safepoint, filled by
    /// `Lowerer::emit_safepoint_map` from the same `cur_bci` the deopt machinery
    /// already keys throw sites on, and passed through `resume_bci` so a
    /// safepoint inside an IR-spliced callee reports the ENCLOSING invoke
    /// rather than a pc that does not exist in this method's code.
    ///
    /// Introspection and diagnosis ONLY -- never read by codegen, the GC root
    /// walk or deopt. It is a translation of an id the GC already keys on, not
    /// a second source of truth about it.
    pub safepoint_bci_table: Vec<(u32, u32)>,
    /// Deoptimization points: native code offsets where deopt can occur.
    /// Used by the deopt framework to reconstruct interpreter state.
    ///
    /// **FROZEN after finalize on a single-pass x64 artifact.** Since r9 wave 6
    /// (`every-x64-deopt-point-is-stored-twice`) this list is the ONLY copy of
    /// those points: the x64 frame-deopt stubs bake the addresses of its
    /// ELEMENTS as imm64s (`x64/deopt_stubs.rs::emit_deopt_stubs`), and
    /// `_deopt_point_boxes` stays empty for them. Mutating an element in place
    /// before install (`stamp_deopt_method_key`) is fine; pushing, removing,
    /// reordering, reassigning or otherwise reallocating this `Vec` once the
    /// artifact exists would leave every baked pointer dangling. The IR tier
    /// still bakes its own boxes and does not depend on this.
    pub deopt_points: Vec<deopt::DeoptimizationPoint>,
    /// `(bci, offset of its OSR entry stub, locals the stub reads)` for every
    /// bci the OPTIMIZING
    /// tier's body can be entered at part-way.
    ///
    /// Filled by `ir_lower` (default on since 2026-09-05;
    /// `CRATONVM_JIT_IR_OSR_ENTRY=0` removes the stubs) and entered by the
    /// interpreter's OSR door through [`Self::ir_osr_enter`] when
    /// [`Self::ir_osr_entry_addr`] names the loop header's bci.
    ///
    /// Distinct from `osr_pc_to_native`, the SINGLE-PASS door's table: that
    /// carries a native offset per bci for a body whose locals live at fixed
    /// homes, and `osr_trampoline` builds the frame around it from outside.
    /// These are entry POINTS — each builds this tier's frame and seeds the
    /// locals itself, because an SSA body has no fixed local→home map to hand
    /// a trampoline.
    pub ir_osr_entries: Vec<(u32, u32, u32)>,
    /// `(bci, one kind byte per JVM local the entry stub at that bci seeds)`,
    /// beside [`Self::ir_osr_entries`] (interpreter round i1 wave 10).
    ///
    /// The stub reads `locals[i]` as the IR type the lowerer planned for local
    /// `i` and nothing checks the interpreter's offer against it (the
    /// single-pass entry has `validate_osr_entry`; this tier had no
    /// equivalent). Byte `i` is [`ir_osr_slot_kind`] of that value's type —
    /// `b'I'`, `b'J'`, `b'F'`, `b'D'`, `b'L'` — or `0` for a local the stub
    /// does not seed (the high half of a two-slot value included) or seeds as
    /// a primitive that is dead at the entry (a reference is always kept: its
    /// word lands in a home the oop maps publish). Asked
    /// through [`Self::ir_osr_entry_kind_mismatch`]. A bci with no row carries
    /// no constraint (a body built before this existed, or by a test).
    pub ir_osr_entry_kinds: Vec<(u32, Box<[u8]>)>,
    /// Round 11 wave 17 (lane tier, `r11w16-irback-osr-opaque-entry-scope-and-door-bit-patch`):
    /// the bcis of [`Self::ir_osr_entries`] whose stub is an OPAQUE arm
    /// (`IrBuilder::build_opaque_osr_merge`). The header's facts include
    /// whatever the interpreter brings, so an entry there is sound whatever
    /// state a handler left, and the OSR door keys its handler-reach refusal
    /// on this per-entry bit instead of the process switch. Ascending, no
    /// duplicates. Empty on every single-pass artifact and whenever
    /// `CRATONVM_JIT_OSR_OPAQUE_ENTRY` is off.
    pub ir_osr_opaque_entry_bcis: Box<[u32]>,
    /// This body emits no site that can return the `i64::MIN` sentinel: no
    /// deopt stub and no call-exception stub.
    ///
    /// Which is a different question from `deopt_points.is_empty()`, and the
    /// difference matters. A deopt POINT is a resume description — the lowerer
    /// records one per safepoint snapshot that has a native offset, so a loop
    /// with no calls at all still has eleven of them. What decides whether a
    /// body can leave abnormally is whether anything JUMPS to a stub, and that
    /// is `deopt_stub_patches` / `call_exc_patches`.
    ///
    /// The OSR door reads it: an optimizing-tier entry has no
    /// `OsrEntryPlan::resume_after_exit` to resume through, so it may only
    /// enter a body that cannot take that exit.
    ///
    /// Except one exit that needs no admission (interpreter round i1 wave 15,
    /// lane L2): an OSR-door compile's back-edge poll mode exit
    /// (`osr_exit::has_poll_mode_exits`), which is taken only when an agent
    /// needs the frame interpreted and resumes at its loop header exactly. It
    /// is not counted here, so it moves no body off this tier.
    pub ir_osr_sentinel_free: bool,
    /// Tier proposal 27 (round 11 wave 10, lane irexc): `Some(bcis)` when the
    /// body emits NO guard deopt, so every exit it has is exceptional — either
    /// a reason-9 rethrow pad (a precise frame the OSR exception transfer
    /// enters the method's own handler with) or a call-exception exit that
    /// publishes nothing. `bcis` are the distinct throw-site bcis of the
    /// latter, already `resume_bci`-mapped (a spliced callee's throw names the
    /// enclosing invoke). `None` when the body has a guard deopt, when a
    /// publishing-nothing exit leaves a snapshot that holds a monitor, and on
    /// every single-pass artifact. `Some` of an empty slice when the body has
    /// no call-exception exit at all (in particular whenever
    /// `ir_osr_sentinel_free`). Read by the optimizing OSR door, which also
    /// requires every one of `bcis` to lie outside the method's exception table
    /// before it enters such a body. `ir_osr_sentinel_free` keeps its meaning,
    /// "no exit at all".
    pub ir_osr_exception_exit_bcis: Option<Box<[u32]>>,
    /// Tier proposal 27, phase 3 (round 11 wave 12, lane irexc): `Some(bcis)`
    /// when every GUARD exit of the body is one the optimizing OSR door may
    /// resume in place with no `OsrEntryPlan`, and nothing an interpreter
    /// continuation of it can do falsifies a fact the body assumes: each guard
    /// is EXACT (a null / bounds / zero-divisor check whose failure makes the
    /// re-executed bytecode at its bci throw) or VALUE-EQUIVALENT (the unboxing
    /// class and layout guards of the final `Integer`/`Long`, whose failure
    /// makes the interpreter compute the value the body would have), and its
    /// point is a single-scope, monitor-free, `REEXECUTE`, resumable frame
    /// (`osr_exit::guard_exit_fact`). `bcis` are the distinct resume bcis of
    /// those guards AND of the call-exception exits (the door requires all of
    /// them outside the method's exception table). `None` when any guard is
    /// speculative (every `Op::Guard`, an unconditional deopt, a non-final
    /// receiver guard), under the conditions that make
    /// `ir_osr_exception_exit_bcis` `None` for a monitor, and on every
    /// single-pass artifact. Equal to `ir_osr_exception_exit_bcis` on a body
    /// with no guard exit. Read by the optimizing OSR door under
    /// `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS=1`.
    pub ir_osr_guard_exit_bcis: Option<Box<[u32]>>,
    /// JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): `true` when this
    /// body wanted OSR entries for its guard exits, its strict guard fact
    /// (`osr_exit::guard_exit_fact`) was refused because some guard is
    /// SPECULATIVE, and the relaxed fact (`osr_exit::guard_exit_fact_opaque`)
    /// admits every exit -- i.e. its exits are sound only behind an opaque
    /// merge at every entry. Set whether or not that relaxed fact was
    /// published. Read only by an OSR-door compile built under the narrowed
    /// opaque policy (`CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR`, `ir_tier`), which
    /// then rebuilds the body with an opaque merge at every header. `false` on
    /// every single-pass artifact.
    pub ir_osr_guard_fact_needs_opaque_entries: bool,
    /// real-frame-deopt: boxed deopt points whose addresses are baked as
    /// imm64 into the guard/deopt-trampoline machine code. JIT code holds raw
    /// pointers into these boxes, so — like `_jit_invoke_infos` — they must
    /// live exactly as long as the code, and they do: they are dropped with
    /// this artifact, which the retirement queue (`defer_jit_owner`) frees only
    /// once no thread can still be executing it. Stable heap addresses (`Box`)
    /// are required: the `deopt_points` Vec above can realloc, these boxes
    /// never move.
    ///
    /// IR tier only since r9 wave 6: a single-pass x64 artifact publishes its
    /// points once, in `deopt_points` (whose elements its stubs bake), and
    /// leaves this empty. Readers that consult both lists already treat this
    /// one as optional.
    pub _deopt_point_boxes: Vec<Box<deopt::DeoptimizationPoint>>,
    /// Round 13 wave 11 (lane chain5, proposal CH-1): the arrival counters the
    /// forced chain deopts of `CRATONVM_DEOPT_EAGER_CHAINS` bump, one per chain
    /// point. Baked into the code as imm64s like [`Self::_deopt_point_boxes`],
    /// so owned for the same lifetime. Empty unless that debug switch is on.
    /// ([`deopt::ForcedChainTripCell`]; since round 16 lane chain the forced
    /// bit rides in the stash, `deopt::claim_forced_trip_mark`).
    pub _forced_chain_deopt_counters: Vec<Box<deopt::ForcedChainTripCell>>,
    /// The point address of each cell above, index for index: the body's chain
    /// points, which publication hands the VM census
    /// ([`Self::forced_chain_points_for_census`], round 16 lane chain).
    /// Round 15 wave 4, lane chain4.
    pub _forced_chain_deopt_points: Vec<u64>,
    /// JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): the countdown cells the
    /// optimizing tier's back-edge polls decrement under
    /// `CRATONVM_DBG_JIT_DEOPT_A_LOT` / `CRATONVM_DBG_GC_AT_BACK_EDGE`
    /// (`ir_lower::Lowerer::emit_poll_stress_countdown`). Baked into the code
    /// as imm64s like [`Self::_forced_chain_deopt_counters`], so owned for the
    /// same lifetime. Empty unless one of those debug switches is set.
    pub _poll_stress_cells: Vec<Box<std::sync::atomic::AtomicU32>>,
    /// JIT round 19 wave 3 (lane eager3, EAGER2-4,
    /// `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`): `(point address, TrapCause code)`
    /// for each point of `_deopt_point_boxes` the optimizing builder planted
    /// for a named cause (an uncommon site trap, a class-init guard). Read by
    /// [`Self::trap_cause_of_point`]. Empty with the switch off and on every
    /// single-pass artifact.
    pub trap_cause_points: Box<[(u64, u8)]>,
    /// JIT round 19 wave 3 (lane eager3, EAGER2-3,
    /// `CRATONVM_JIT_UNINIT_NEW_GUARD`): the init words
    /// (`Class::init_state`) the body's class-init guards read at a baked
    /// address (`ir::Op::ClassInitTest`), owned for the body's lifetime as
    /// `_jit_invoke_infos` are. Empty unless such a guard was planted.
    pub _jit_class_init_cells: Vec<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    /// NEW-12: precise oop maps indexed by native PC offset.
    ///
    /// Each entry records the frame-slot offsets (relative to RBP)
    /// that hold live object references at a specific safepoint. The
    /// GC root walker uses this list to enumerate real oops rather
    /// than conservatively treating every stack word as a root.
    ///
    /// The entries are kept sorted by `native_pc_offset` so that
    /// [`Self::find_oop_map_for_pc`] can do an O(log n) binary
    /// search at GC time. Direct `push_oop_map` appends and then
    /// resorts on the next call to `find_oop_map_for_pc` — the
    /// amortized cost is negligible because per-method entries are
    /// typically single-digit.
    ///
    /// An empty `oop_maps` vector means "no precise coverage" and
    /// the root walker falls back to the conservative stack scan.
    /// Both x64 compile paths populate it at GC-capable safepoints (the
    /// single-pass `x64/safepoint.rs` and `ir_lower.rs`, installed by
    /// `x64/driver.rs` and the IR finalize), as does the AArch64 backend; a
    /// body is empty only when it has no such safepoint or its maps were
    /// incomplete.
    pub oop_maps: Vec<OopMapEntry>,
    /// gen r5w2/oomjit6: the safepoint ids ([`OopMapEntry::bytecode_pc`],
    /// sorted, distinct) whose map kept its `reg_oop_mask` although a
    /// reference was staged where no map names it -- the native outgoing
    /// area, the direct-call service slots or an inlined callee's parameter
    /// locals. Non-empty only under `CRATONVM_GC_STAGED_ARGS_KEEP_MASK` on the
    /// single-pass x64 backend, which then also publishes
    /// `frame_layout.outgoing_lo == 0` ("no claim") for the method. A scanner
    /// that honours this list can keep the outgoing reserve at exactly these
    /// safepoints instead.
    pub staged_args_unmapped_sp_ids: Vec<u32>,
    /// gen r5w3/live7: the per-safepoint liveness claims over this body's
    /// frame words (see [`FrameLivenessTable`]). Empty -- no claim -- unless
    /// the optimizing tier compiled the body under
    /// `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`; the collector's band scan spends
    /// whatever table a body carries, so that per-compile read is the gate.
    pub frame_liveness: FrameLivenessTable,
    /// RBC.2 — `true` when this artifact was produced by the OSR compile
    /// path (which eagerly compiles invokestatic callees and wires direct
    /// calls). The OSR trigger only REUSES artifacts with this flag: a
    /// first-call/upgrade artifact may lack that wiring, and pinning it
    /// into a hot loop forever (instead of one fresh OSR compile) routes
    /// every callee through the slow dispatch helper.
    pub compiled_via_osr: bool,
    /// The bytecode pc this OSR artifact was COMPILED FOR, when it was.
    ///
    /// `osr_pc_to_native` carries an offset for every pc the codegen accepted
    /// as an entry, so one artifact can legitimately be entered at several loop
    /// headers — and `can_osr_enter` allows exactly that. This field records
    /// which pc the compile was actually requested for, so
    /// `CRATONVM_JIT_OSR_SINGLE_PC=1` can restrict entry to it and answer
    /// whether a multi-pc entry is sound, without guessing from a trace.
    pub osr_compiled_entry_pc: Option<usize>,
    /// RBC.5 — raw class ids of the declaring classes of every
    /// getstatic/putstatic site in this method, recorded at compile time
    /// from the already-resolved `static_field_info`. JIT code reads static
    /// storage directly, so these classes must be initialized before first
    /// execution; recording them here lets the interpreter's compiled-entry
    /// fast path run that check once per artifact instead of re-resolving
    /// every constant-pool ref on every call.
    pub static_init_classes: Vec<u32>,
    /// RBC.5 — set once the ensure-initialized walk over
    /// `static_init_classes` has fully succeeded for this artifact.
    pub static_inits_done: std::sync::atomic::AtomicBool,
    /// NEW-12: cached flag — `true` once `oop_maps` is known to be
    /// sorted by `native_pc_offset`.
    ///
    /// This exists purely to keep the O(n) sortedness scan off the
    /// per-safepoint GC lookup path. `find_oop_map_for_pc` reads it: when
    /// `false` it runs the `windows(2)` check once (sorting only if the
    /// data is actually out of order) and then sets it `true`, so every
    /// subsequent lookup skips the scan entirely. It starts `false`
    /// because the production codegen path moves a fully-built vector in
    /// wholesale (`cm.oop_maps = compiler.oop_maps`, bypassing
    /// `push_oop_map`); any `push_oop_map` likewise clears it so the next
    /// lookup re-verifies.
    pub(crate) oop_maps_sorted: bool,
    /// Stage 3 (precise oop maps) — frame offset (positive; slot at
    /// `[rbp - sp_id_slot_off]`) where the JIT stored the active
    /// safepoint's bytecode PC before each GC-capable call. The GC root
    /// walker reads this slot to recover the exact `OopMapEntry`. `0`
    /// means the precise gate (`CRATONVM_PRECISE_JIT_MAPS`) was off at
    /// compile time, so no safepoint-id slot exists and the walker uses
    /// the conservative path for this method.
    pub sp_id_slot_off: i32,
    /// No word of this method's own frame below the callee-saved save area is
    /// ever a reference: the body has no reference-typed value, no reference
    /// parameter and no shadow frame block, and every one of its
    /// [`Self::oop_maps`] names no frame slot. Default `false`; set only by the
    /// optimizing tier (`ir_lower`, calls proposal 1 /
    /// `r11w13-fib-oop-free-frame-contract-patch`).
    ///
    /// It is a FRAME CONTRACT, not an optimisation hint. A frame with it set
    /// may leave its safepoint-id slot (`sp_id_slot_off`) and its reserved-tail
    /// words unwritten, so the collector must not read that slot for a GC
    /// purpose: `vm/src/jit/conservative_roots.rs` treats such a frame as
    /// covered and root-free without resolving a map, and never lets a stale
    /// id there select one. The callee-saved GPR image is NOT covered by the
    /// claim -- it holds the CALLER's registers and is rewritten as for any
    /// frame.
    ///
    /// The slot is still a stack-trace input, under a narrower rule the
    /// emitter must keep: the id is stored before every call that does not
    /// enter a compiled frame (a runtime helper, a dispatch stub), so the
    /// INNERMOST activation's slot is current, and a frame suspended in a
    /// direct JIT-to-JIT call is placed by [`Self::return_site_bci_table`]
    /// instead.
    pub frame_holds_no_references: bool,
    /// JIT round 25 (item 3 of the round-24 redesign,
    /// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`, default ON): the frame offsets
    /// (`[rbp - off]`), ascending, of every spill colour the optimizing tier's
    /// slot colouring proved never holds a reference of this activation (its
    /// `Prim` colours, `ir_lower::SlotPlan::prim_colors`). Empty -- no claim --
    /// for every other artifact and whenever the switch was off at compile
    /// time; the collector reads only this, never the switch.
    ///
    /// It is a FRAME CONTRACT for the whole body, not per safepoint: the
    /// emitter may leave these words out of the prologue's fresh-frame clear,
    /// so at ANY point the thread can be stopped -- the entry poll and every
    /// other point with no current safepoint id included -- such a word holds a
    /// primitive this activation wrote or the previous stack occupant's
    /// residue. The band scan (`vm/src/jit/conservative_roots.rs`,
    /// `ir_prim_colour_claim`) therefore never roots an object start there,
    /// exactly as it treats `OopMapEntry::non_oop_stack_slots` at a safepoint
    /// whose map resolved (the same list).
    pub ir_prim_colour_claim: Box<[i16]>,
    /// `CRATONVM_DBG_JITC` only: `(bytes the prologue's fresh-frame clear
    /// would write without` [`Self::ir_prim_colour_claim`]`, bytes it writes
    /// with it)`. `(0, 0)` unless the claim and the debug switch were both on.
    pub ir_prim_colour_clear_bytes: (u32, u32),
    /// JIT round 25 wave 2, `CRATONVM_DBG_JITC` only: the optimizing tier's
    /// frame by region and its fresh-frame clear, before and now
    /// (`ir_lower::Lowerer::frame_anatomy`), printed at publication as the
    /// `ir-frame-anatomy` line with the method's name. Empty otherwise.
    pub ir_frame_anatomy: String,
    /// `(native offset of a return address, bci)` for every call of a
    /// [`Self::frame_holds_no_references`] body, ascending by offset. Empty for
    /// every other artifact.
    ///
    /// The bci is the one [`Self::safepoint_bci_table`] would give the call's
    /// safepoint (already `resume_bci`-mapped for a call inside a spliced
    /// body). A stack walk reads it for a frame BELOW the innermost one, whose
    /// return address it holds exactly; see `activation_bci` in
    /// `vm/src/jit/conservative_roots.rs`. Introspection only, like
    /// `safepoint_bci_table`.
    pub return_site_bci_table: Vec<(u32, u32)>,
    /// Stage A (precise oop maps, B-K fix) — `true` only when EVERY
    /// GC-capable safepoint in this method has a precise oop map AND the
    /// method has no coverage-breaking construct (OSR entry, un-mapped
    /// inlined-callee safepoint). When `true` the GC root walker may
    /// skip the conservative backstop sweep for this frame and treat its
    /// precise oops as relocatable (movable) rather than pinned — the
    /// change that lets selective promotion drain JIT-rooted young
    /// objects without the B-K stale-slot corruption. `false` (the
    /// default) preserves the conservative, pin-everything behaviour, so
    /// it is always safe to leave unset. Only ever consulted on the
    /// gated precise path (`CRATONVM_PRECISE_JIT_MAPS`).
    pub fully_oop_covered: bool,
    /// Every safepoint of this method claims **shadow** coverage — the method
    /// has at least one `OopMapEntry` and all of them carry
    /// `moving_young_coverage_complete`.
    ///
    /// A DIFFERENT question from [`Self::fully_oop_covered`], and the
    /// difference is what
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` ran
    /// aground on. `fully_oop_covered` asks whether every live oop is named by
    /// a FRAME SLOT in the map (`safepoint_pcs ⊆ mapped_safepoint_pcs`), which
    /// a direct JIT→JIT call with a reference argument can never satisfy: the
    /// argument is marshalled into the outgoing-ABI area, which no frame-slot
    /// map can name. `fully_shadow_covered` asks whether the shadow push
    /// published every live oop so a relocation can rewrite it — the property
    /// `conservative_roots::moving_young_frame_coverage_complete` already
    /// consults per frame, per safepoint, for every non-OSR frame.
    ///
    /// Measured on `TestKillProcessWhileWriting`, 2026-08-23: 439 of the 449
    /// recorded coverage failures were `staged_arg_unmappable`, i.e. exactly
    /// the direct-call shape above, and they blocked relocation on 725 of 759
    /// collections through the OSR fallback's use of `fully_oop_covered`.
    pub fully_shadow_covered: bool,
    /// deopt-osr scaffolding — `true` only once the deopt finalizer has
    /// proven this method can rebuild a precise interpreter frame at a guard
    /// bci and resume there (instead of the `i64::MIN` whole-method re-run).
    /// `false` (the default) keeps the method on the safe re-run path; no
    /// emitter populates it yet, so it is currently always `false`. Mirrors
    /// the `fully_oop_covered` coverage-gate pattern: purely additive, no
    /// behaviour change until the resume path is wired
    /// (see `docs/feature-designs/deopt-osr.md`).
    pub can_deopt_resume: bool,
    /// Every potentially suspending bytecode has an interpreter transfer before it.
    pub vthread_boundary_guards: bool,
    /// `true` when **no body this artifact spliced commits a side effect**
    /// — no array store, no `putfield`/`putstatic`, no invoke, no monitor
    /// action anywhere in the relocated bytecode.
    ///
    /// Vacuously `true` for an artifact that spliced nothing, which is why
    /// the default is `true` rather than `false`: the interpreter pairs it
    /// with a check of the method's own bytecode PREFIX, and a method with
    /// no splices is fully described by that prefix.
    ///
    /// What it is for: a deopt taken inside a spliced body abandons an
    /// attempt that ran the caller's bytecode up to the enclosing invoke
    /// and then part of the callee's. The interpreter may re-run the whole
    /// method only if neither half can have committed anything observable.
    /// The prefix answers the first half; this answers the second.
    ///
    /// Since JIT r22 wave 8 a FIRST-ARRIVAL point at an own non-invoke bci
    /// does not read it (`deopt::replay_own_site_ignores_splice_purity`,
    /// `CRATONVM_JIT_TRAP_REPLAY_OWN_SITE_IGNORES_SPLICES`): its first-arrival
    /// walk counts every invoke, so no spliced body can have run.
    pub spliced_bodies_side_effect_free: bool,
    /// Round 13 wave 6 (lane chain2; `r13w5-resume-chain-scope-class-ids-patch`):
    /// `(method key, class id)` of every body the optimizing tier spliced,
    /// sorted by key, one row per key (`ir::splice_scope_class_ids`). The VM
    /// resolves an inlined deopt chain's inner scope by this id
    /// (`deopt::splice_scope_class_hint`) instead of by name from the
    /// enclosing class's loader, which can miss a substituted owner or find
    /// another class of the same name. Filled only when the compile records
    /// chain snapshots (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`, the one
    /// producer of optimizing-tier chains); empty everywhere else.
    pub splice_scope_class_ids: Vec<(String, u32)>,
    /// Round 14 wave 3 (lane chain, R14DP-5): the bytecode of every body the
    /// optimizing tier spliced, sorted by key, one row per key
    /// ([`SpliceScopeSource`]), for the VM's doors to rebuild an inner chain
    /// scope of a class redefined since the compile. Filled under the same
    /// condition as [`Self::splice_scope_class_ids`], only with
    /// [`chain_inner_own_source_enabled`] and when no redefinition began
    /// during the compile ([`splice_sources_read_at_compile_stamp`]); empty
    /// everywhere else.
    pub splice_scope_sources: Vec<SpliceScopeSource>,
    /// JIT round 19 wave 3 (lane splice3, SPLICE2-3 of
    /// `jit-r19-splice2-proposals`): the bcis of this method's OWN calls the
    /// optimizing tier registered as guarded receiver splices (W6-1 / GS-1 /
    /// CH5-5: a class test, the body on its hit edge, the call -- or a trap --
    /// on the miss edge). Sorted, deduplicated; empty for every other
    /// artifact. The inline cache at such a bci sits on the MISS edge and sees
    /// only the receivers the test rejected, so it is no evidence of which
    /// class dominates the site ([`Self::dominant_receiver_at_bci`] answers
    /// `None` there).
    pub ir_guarded_splice_bcis: Box<[u32]>,
    /// JIT round 19 wave 11, lane tail11 (TAIL9-20-2, default on
    /// `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`): the hit/miss counters of
    /// every single-pass guarded nested splice this body carries, by site.
    /// Handed to the [`JitCache`] on publication
    /// ([`JitCache::sp_nested_miss_counts`]). Empty for every compile with the
    /// switch off.
    pub sp_nested_miss_cells: Box<[SpNestedMissEvidence]>,
    /// JIT round 19 wave 12, lane tail12 (TAIL11-2): set once by
    /// [`JitCache::claim_sp_nested_stuck_bodies`] when one of this body's
    /// guarded nested splices crossed the TAIL9-20-2 bar, so the body is
    /// withdrawn for a recompile at most once.
    pub sp_nested_withdraw_claimed: std::sync::atomic::AtomicBool,
    /// JIT round 19 wave 4 (lane splice4, SPLICE3-1): `(bci, class id)` for
    /// the bcis of [`Self::ir_guarded_splice_bcis`], the class each guarded
    /// receiver splice TESTED there (its hit edge's class: the constant-pool
    /// class or the profile's dominant one). Sorted by bci, one row per bci
    /// (the first registered class), class `0` never stored. Read only through
    /// [`Self::guarded_splice_class_at_bci`], by `nest-profiled` under
    /// `CRATONVM_JIT_IR_NESTED_GUARD_CLASS_EVIDENCE`; recorded always (data
    /// only).
    pub ir_guarded_splice_classes: Box<[(u32, u32)]>,
    /// JIT round 19 wave 4 (lane splice4, SPLICE3-3): marked expected-throw
    /// trips this body took (`ResumeSemantics::expected_throw`), counted by
    /// the VM's sinks only under `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_RATE_LIMIT`
    /// (`ir::expected_throw_trip_rate_exceeded`). Saturating; `0` otherwise.
    pub splice_expected_throw_trips: std::sync::atomic::AtomicU32,
    /// deopt-osr Step 7 — `true` once the OSR-exit map emitter has recorded at
    /// least one loop-boundary exit map for this (OSR-compiled) method, i.e. it
    /// can leave a running JIT/OSR frame mid-loop at a loop bci with the loop's
    /// live state, rather than the `i64::MIN` re-run (which is *wrong* for an
    /// OSR'd frame entered partway through). Set at finalize to
    /// `!osr_exit_points.is_empty()`; only non-empty when `deopt_real_enabled()`
    /// was on at compile, so `false` in production. Step 8 consults it (with the
    /// `CRATONVM_DEOPT_REAL` gate) before routing a mid-loop bail.
    pub can_osr_exit: bool,
    /// jit-invokedynamic-groovy-regression fix — `true` when this artifact
    /// compiled at least one `invokedynamic` (0xba) site, i.e. it contains an
    /// UNCONDITIONAL reason-8 uncommon trap that fires on every execution
    /// reaching that site. Entry-publication gates (JIT→JIT direct-call
    /// baking, MIC/PIC inline-cache installs) consult this so machine code
    /// never calls such an artifact directly — every call stays on a dispatch
    /// helper, which can resolve the trap precisely in place
    /// (`try_resume_trapped_callee`). Set at x64 finalize from
    /// `!compiler.indy_info.is_empty()`; `false` for IR-path artifacts (the
    /// IR lowerer rejects invokedynamic methods).
    pub has_indy_trap: bool,
    /// `true` when the single-pass emitter put down at least one uncommon-trap
    /// stub with no reconstructed frame (`emit_deopt_stubs`' fall-through to
    /// `jit_uncommon_trap`). Such an exit leaves an OSR body with nothing to
    /// transfer, so `try_osr` can only replay the loop from the entry pc
    /// (r11w5-tier-frameless-trap-osr-refusal-patch, step 1: the bit and its
    /// census; the OSR refusal is step 2). `false` for IR-path artifacts,
    /// whose traps always stash a frame.
    pub has_frameless_trap_stub: bool,
    /// `true` when every frameless trap stub of this body sits where a re-run
    /// from entry commits nothing twice
    /// (`deopt::replay_from_entry_commits_nothing` at each of the emitter's
    /// `frameless_trap_bcis`), stamped by the single-pass finalizer. The
    /// method-entry doors' census answers a bare sentinel out of such a body
    /// `frameless replay=exact` instead of asking the whole-body rule, which
    /// calls every storing method unsound and retires it
    /// (round 12 wave 7, lane replay4;
    /// `r12w7-replay4-frameless-door-rerun-retires-exact-replays`). `false`
    /// (the whole-body rule) until stamped.
    pub frameless_traps_replay_exact: bool,
    /// Memoised [`Self::osr_exit_policy`] verdict.
    ///
    /// That function walks EVERY deopt point of the artifact, and per point
    /// walks the whole frame state (`deopt::first_unresumable_slot`). It is
    /// asked once per OSR ENTRY, from `validate_osr_entry` — and its answer is
    /// a pure function of `deopt_points`, which is immutable after publication.
    ///
    /// Recomputing it was 3.3% of a throw-heavy OSR loop under `perf record`
    /// (`probes/OsrExcRateProbe.java`), because since the RBC.6b lift a caught
    /// exception is an OSR exit plus a re-entry — so a `try`/`catch` loop pays
    /// one full policy walk per throw, over a point list that just grew a
    /// reason-9 entry per protected invoke.
    pub(crate) osr_exit_policy_memo: std::sync::OnceLock<Result<OsrExitPolicy, bailout::Bailout>>,
    /// deopt-osr Step 7 — the loop-boundary bcis (OSR-vetted, outside every
    /// LICM-hoisted body) for which an OSR-exit map was emitted into
    /// `deopt_points` (tagged `DeoptReason::OsrExit`). Empty unless
    /// `deopt_real_enabled()` was set at compile. Step 8 looks a trapping loop
    /// bci up here to decide whether to OSR-exit (resume the loop body) vs
    /// re-run.
    pub osr_exit_points: Vec<usize>,
    /// The bcis (sorted, unique) of the single-pass post-call exit points
    /// that bake their own stub (`x64/op_invoke.rs`
    /// `PostCallExitClaimOutcome::OwnBeside`, interpreter round i2 waves
    /// 55-56): an `OsrExit` point in `deopt_points` beside a guard of another
    /// frame, deliberately NOT in [`Self::osr_exit_points`]. The OSR door's
    /// by-bci readers ask this list so such a point is neither the
    /// `ExitMapMissing` cross-check (`classify_osr_exit_site`) nor the entry
    /// contract at its bci (`osr_entry_frame_state`). Empty unless
    /// `CRATONVM_JIT_POST_CALL_OWN_POINT_EXITS` settled a claim that way.
    pub post_call_own_exit_bcis: Vec<usize>,
    /// deopt-osr scaffolding — monotonic compilation epoch for this artifact.
    /// When `MakeNotEntrant` invalidation lands, boxed `DeoptimizationPoint`
    /// pointers (baked into guard code) are versioned by this epoch so a
    /// resume never follows a box belonging to a superseded compilation.
    /// `0` for every artifact today (no invalidation consumer yet).
    pub compilation_epoch: u64,
    /// deopt-osr Step 9 follow-up (a) — raw pointer to this artifact's retained
    /// [`crate::deopt::DeoptEpochGuard`], baked as the 4th arg into every
    /// frame-deopt stub. Null on production artifacts (a guard is allocated only
    /// when a frame-deopt stub is emitted, which requires `deopt_real_enabled()`).
    /// The VM stamps it (creation epoch + stable live-epoch cell) at install via
    /// [`Self::stamp_deopt_epoch_guard`]; `x64_deopt_entry` then reads it BEFORE
    /// dereferencing the box. (The before-deref short-circuit that used this
    /// belonged to a `CRATONVM_JIT_FREE_CODE=1` mode that freed boxes under a
    /// running frame. That mode is gone: an executing artifact owns its boxes
    /// until it returns.) The pointed-to guard is leaked (process-lifetime), so
    /// this raw pointer is always valid.
    pub deopt_epoch_guard: *const crate::deopt::DeoptEpochGuard,
    /// This body is an `ACC_SYNCHRONIZED` method's, so entering it is only
    /// legal through a caller that supplies the implicit monitor.
    ///
    /// A compiled body carries no monitor prologue/epilogue — the CALLER
    /// supplies it. Exactly two entry points do: `execute_jit_call` and
    /// `execute_jit_call_decoded`, which wrap the activation in a
    /// `JitSynchronizedMonitorGuard`. Every other consumer CALLs the raw entry
    /// pointer, and for one of those a synchronized method simply does not
    /// lock: `RSyncMethodJit` lost ~35 of 240 000 monitor-protected increments
    /// per run once `bumpStatic` was both published and reached from compiled
    /// code.
    ///
    /// The refusal used to live only in `try_jit_compile_callee_slow`, which
    /// is the COMPILE path. Its by-name caller `try_jit_compile_callee` serves
    /// an already-published body straight out of `jit_cache` without re-asking,
    /// and `jit_invoke_dispatch`'s own `jit_cache` arm never went through
    /// either — so a body legitimately published FOR the wrapped entry (the
    /// background tiering door passes `allow_synchronized_wrapped_entry`) was
    /// handed to unwrapped ones. A predicate that only guards the slow path
    /// guards nothing once the fast path can answer.
    ///
    /// Stamped at publication beside [`Self::owner_class_id`], so it travels
    /// with the body to every door that holds only a raw entry pointer.
    pub requires_wrapped_entry: bool,
    /// Round 13 wave 4 (lane sync2): this body is an `ACC_SYNCHRONIZED`
    /// instance method's SELF-LOCKING body -- its prologue enters the
    /// receiver's monitor and every exit releases it (`x64/frames.rs`
    /// `emit_self_lock_enter`), HotSpot's shape -- so a raw CALL to its entry
    /// is a correctly locked call. [`JitCache::put`] therefore publishes it
    /// with [`Self::requires_wrapped_entry`] cleared, whatever the door
    /// stamped, and every raw consumer (inline caches, the dispatch cache,
    /// not-entrant forwarding, cycle cells) may call it. A door that wraps it
    /// anyway (`execute_jit_call`'s monitor guard) only re-enters the monitor
    /// recursively. `false` for every other body.
    pub self_locks_monitor: bool,
    /// JIT round 19 proposal wave 6, lane sync6 (SYNCDOOR618-5): the hold a
    /// self-locking body keeps ([`SelfLockHold`]); `Some` exactly when
    /// [`Self::self_locks_monitor`] is set by a backend that knows the shape.
    /// Read by the VM's locked-monitor attribution
    /// (`CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS`).
    pub self_lock_hold: Option<SelfLockHold>,
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the `synchronized`
    /// BLOCKS a SINGLE-PASS body holds at each safepoint id, as `(emitter pc,
    /// locals holding the lock objects, outermost first)` sorted by pc -- the
    /// emitter's `taken_monitor_slots_at` (every path agrees, every level a
    /// taken lock naming its local), kept only for the pcs an oop map is
    /// recorded at. Local `k`'s home is `[rbp - 8 * (k + 1)]` in a single-pass
    /// frame ([`FrameLayout::java_locals_hi`]); a reader reads it only where
    /// the active oop map names that home (so a moving collection rewrote it).
    /// Empty for the optimizing tier, for a body with no monitor bytecode and
    /// for a body the analysis cannot describe. Metadata only: no emitted code
    /// reads it. See [`Self::taken_monitor_locals_at`].
    pub taken_monitor_slots: Box<[(u32, Box<[u16]>)]>,
    /// JIT round 19 proposal wave 8, lane gc8 (GC719-1): the `synchronized`
    /// BLOCKS an OPTIMIZING-tier body holds at each safepoint id, as
    /// `(safepoint id, frame words holding the lock objects, outermost
    /// first)` sorted by id. A word is a positive RBP offset (`[rbp - off]`):
    /// the lock object's authoritative reference home, which the oop map
    /// recorded at that id names (`ir_lower`, `emit_safepoint_map`). Recorded
    /// only where every lock the compiled code took there has such a home
    /// (all or nothing); never for a lock the optimizer elided, for the method
    /// monitor of a self-locking body ([`Self::self_lock_hold`] names that
    /// one), or at a point whose snapshots disagree. Empty for the
    /// single-pass tier ([`Self::taken_monitor_slots`] is its table) and for a
    /// body holding no block. Metadata only: no emitted code reads it. See
    /// [`Self::ir_monitor_homes_at`].
    pub ir_monitor_homes: Box<[(u32, Box<[i32]>)]>,
    /// Set by a single-pass compile door that must not replace a live
    /// optimizing body (tier proposal R12-4): [`JitCache::put`] then keeps a
    /// published, non-retired `used_ir_backend` body under the key and drops
    /// this one. `false` (every other door) publishes as `put` always has.
    pub keep_live_optimizing_body: bool,
    /// `ClassId` of the class this body was published under, or
    /// [`cratonvm_types::jit_activation::NO_OWNER_CLASS`] for an artifact that
    /// was never published (test fixtures, probe bodies).
    ///
    /// Stamped by [`JitCache::put`] / [`JitCache::put_osr`] from the same
    /// `declaring_class_id` they key the artifact on. It replaces the
    /// `entry_ptr -> class_id` map `jit_activation` used to keep under a global
    /// lock: every activation site already holds the `&CompiledMethod`, so the
    /// lookup was pure overhead — and reading the owner off the live artifact
    /// cannot return a retired entry's stale answer.
    pub owner_class_id: u32,
    /// `ClassId` of the class that DECLARES the compiled method, when that is
    /// not the class [`Self::owner_class_id`] names, else
    /// [`cratonvm_types::jit_activation::NO_OWNER_CLASS`].
    ///
    /// The by-name callee door publishes an inherited method's body under its
    /// RECEIVER's key (`try_jit_compile_callee_slow`'s recompile-storm fix:
    /// `ClassLoaders$PlatformClassLoader.loadClass(String,boolean)` for the
    /// body of `BuiltinClassLoader.loadClass`), so `owner_class_id` is the
    /// subclass. Every reader of a live frame's class -- the stack trace's
    /// source file and line, the caller-class walk, JDWP rows, the static
    /// mirror of a held monitor, the redefinition-staleness check -- wants the
    /// method's holder, as HotSpot reports it. Read through
    /// [`Self::frame_class_id`]; stamped by that door before `put`
    /// (`i74-L2-l2w73-throwable-trace-shapes-jdk-only-diff`: a compiled
    /// `BuiltinClassLoader.loadClass` frame read `(ClassLoaders.java)` with no
    /// line). Cache keys, the loader keep-alive and supersede checks keep
    /// reading `owner_class_id`.
    pub holder_class_id: u32,
    /// This artifact's compile-id — the identity its prologue publishes into
    /// the TLS mirror beside RBP, so a conservative scan can name the method
    /// owning the innermost frame instead of decoding the call that created it.
    /// Reserved before codegen (the immediate is encoded in the prologue),
    /// bound to this artifact at publication, released on drop. 0 = the mirror
    /// is unavailable and nothing is published for this method.
    pub compile_id: u32,
    /// Value of [`jit_install_epoch`] when this artifact's COMPILATION began.
    ///
    /// The install-side half of the redefinition hole. `redefineClass` bumps
    /// the epoch and then flushes the cache
    /// (`vm/src/vm/vm_exec.rs::redefine_class`), and a synthetic-stub layout
    /// upgrade flushes it too (`vm/src/vm/vm_init.rs::jit_invalidate_adapter`).
    /// Neither reaches a compilation that is ALREADY RUNNING: the broker has no
    /// epoch on its queued or in-flight requests, so a compile that read the
    /// pre-redefinition bytecode completes afterwards and — until this field
    /// existed — was published unconditionally and called. The published body
    /// then executes bytecode the agent replaced, or (for a layout upgrade)
    /// reads field offsets that no longer describe the class.
    ///
    /// [`JitCache::put`] / [`put_osr`](JitCache::put_osr) compare this against
    /// the owning cache's `flush_barrier` and refuse a stale artifact. Stamped
    /// in both constructors from [`current_compile_install_epoch`], which reads
    /// the compile-wide thread-local witness [`open_compile_epoch_witness`]
    /// opens (so the stamp is the epoch at compile START, not at finalize).
    pub install_epoch: u64,
    /// Value of the process invalidation epoch when this artifact's
    /// COMPILATION began, stamped through the same witness as
    /// [`Self::install_epoch`].
    ///
    /// The dependency-side half of the same hole. A background compile that
    /// inlined or devirtualised `Base.m()` can finish after
    /// `invalidate_for_class_change("Base")` has scanned a cache that did not
    /// contain it yet; nothing would ever withdraw it. [`JitCache::put`] /
    /// [`put_osr`](JitCache::put_osr) check every invalidation that cache logged
    /// after this stamp against the body's `inlined_methods` and key, and
    /// refuse on a match.
    pub invalidation_epoch: u64,
    /// Process-unique identity for this artifact.
    ///
    /// Neither the entry ADDRESS nor the `CompiledMethod`'s own address
    /// identifies a body: an executable buffer is recycled to the next
    /// compile of a similar size, and the `Arc`'s allocation is recycled by
    /// the global allocator just as readily. A check built on either is an
    /// ABA -- it compares equal across two different artifacts. This is a
    /// monotonic counter, so it never repeats.
    ///
    /// Read by [`JitCache::prepare_for_publication`], which has to answer
    /// "is the body at this baked address still the one it was baked for",
    /// not merely "is something alive there".
    pub artifact_id: u64,
    /// Set once an invalidation has withdrawn this body from dispatch — the
    /// class-hierarchy, redefinition or unloading assumptions it was compiled
    /// under no longer hold. A body merely SUPERSEDED by a tier-up is not
    /// retired: it is still correct code.
    ///
    /// Stored under the owning cache's `mutation` lock, BEFORE that cache
    /// clears any inline cache. Every inline-cache publication re-reads it
    /// after publishing (`JitMICSlot::update`, `JitPICSlot::install`) and rolls
    /// the publication back, so a helper that resolved this body just before
    /// the invalidation cannot re-install it behind the clearing pass.
    pub retired: std::sync::atomic::AtomicBool,
    /// Set once a later publication of the same key replaced this body in
    /// [`JitCache::put`]. NOT an invalidation: the body is still correct code,
    /// and every `retired` reader (`prepare_for_publication`, the cycle and
    /// retire cells, the lambda adapter, the code-cache sweep) must keep
    /// ignoring it. Read by the VM's two thread-local memos that hold a
    /// compiled body (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`), which drop an
    /// entry naming a superseded body at its next hit — which is what lets a
    /// replacement move only the publish half of the generation word instead
    /// of flushing every thread's positive memos (round 11 wave 11, from
    /// `r11w10-rt-lib-superseded-body-memo-patch`).
    pub superseded: std::sync::atomic::AtomicBool,
    /// The inline-cache slots that published THIS body as a call target: the
    /// reverse index [`JitCache::put`] walks when a later publication of the
    /// same key supersedes it (round 11 wave 19, lane lhm,
    /// `r11w18-hashmap-inline-caches-keep-a-superseded-callee`). See
    /// [`IcHolderRef`] and [`CompiledMethod::retarget_superseded_inline_caches`].
    ///
    /// Filled by [`JitMICSlot::update`] and [`JitPICSlot::install`] (and by
    /// [`bind_inline_cache_holders`] for a slot that published before its own
    /// body was bound), only for slots whose holder is bound. Entries are never
    /// trusted: each names its holder through a `Weak` and is re-checked
    /// against the slot's current owner before anything is cleared, so a stale
    /// entry costs one failed upgrade or one compare, never a wrong clear.
    pub(crate) ic_holders: parking_lot::Mutex<Vec<IcHolderRef>>,
    /// How many Java arguments (receiver included, one per value, the
    /// compact ABI) this body's method entry takes, or `u16::MAX` when the
    /// producer did not say. Stamped by the single-pass driver and by
    /// `try_compile` for both tiers; with the descriptor it tells a static
    /// method from an instance one, which the not-entrant record needs
    /// ([`Self::make_not_entrant`], interpreter round i1 wave 22, lane L6).
    pub entry_java_args: u16,
    /// gcd d9/d (2026-09-28): the method this body compiles declares no
    /// exception table (and, where the producer knows it, is not
    /// `ACC_SYNCHRONIZED`). Stamped by [`try_compile_request`] for both tiers'
    /// method-entry bodies and by the single-pass driver for every door into
    /// it (the eager first-call door included) from the staged table; `false`
    /// ("not known") for every other producer, which is the conservative
    /// answer. A synchronized method's body is never a plain direct-call
    /// target anyway (the bind refuses it, and its self-locking shape is
    /// excluded by name). It is one of the facts `callee_body_owns_its_arguments`
    /// asks before a baked single-pass direct CALL into this body keeps no
    /// copy of the call's arguments: the call site's callee-deopt service
    /// could otherwise be asked to run a handler of this method, and that
    /// needs the arguments.
    pub method_declares_no_handlers: bool,
    /// gkr (OOM21-2, the CALLER half of [`service_copy_window_enabled`]): per
    /// direct call that keeps a copy of its reference arguments for the
    /// callee-deopt service, the call's return offset past [`Self::entry_ptr`]
    /// and the frame slots (`[rbp - off]`, the `frame_slot_offsets`
    /// convention) of those copies. Nothing but the service reads them after
    /// the CALL. Sorted by offset; empty when the producer recorded none.
    pub service_arg_copies: Vec<(u32, Box<[i16]>)>,
    /// gkr (OOM21-2, the CALLEE half): the safepoint ids of this body at which
    /// no point a re-run from entry would be exact is reachable any more
    /// ([`crate::deopt::rerun_window_closed_at`]): a trap from here on is
    /// refused rather than re-run, so the caller's service copies have no
    /// reader. Sorted; empty when the producer computed none.
    pub rerun_window_closed_sp_ids: Vec<u32>,
    /// Safepoints where every incoming reference parameter slot is dead and
    /// this body promises a precise deopt resume. Service-copy roots may be
    /// transferred to the callee at these points, but a declined frame must
    /// never replay from entry after that transfer.
    pub service_arg_resume_only_sp_ids: Vec<u32>,
    /// RE-ENTRY-ONLY publication (interpreter round i6 wave 117, lane L2,
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`; stamped by [`JitCache::put`]
    /// for a [`JitCache::mark_reentry_only`] key): [`JitCache::get`] never
    /// answers this body, only [`JitCache::get_for_reentry`] does, so no
    /// compiled or interpreted caller binds to it and every call keeps
    /// reaching the method's registered native, while the natives'
    /// bytecode-only re-entries run it compiled.
    pub reentry_only: bool,
    /// Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`,
    /// stage 2 of `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// for a compile made while a debugger may read locals, `(bci, one entry per
    /// local)` at each top-level safepoint, ascending by bci. Each entry says
    /// what the local's home slot `[rbp - (idx + 1) * 8]` holds there; the
    /// safepoint's spill stored every register- and XMM-homed local to it.
    /// Empty for every other compile.
    pub debugger_home_locals: Vec<(u32, Box<[DebuggerHomeLocal]>)>,
    /// Interpreter round i6 wave 121, lane L2 (stage 3a of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// for a compile made while a debugger may read locals, the bci of every
    /// call (an invoke, or a `getstatic` / `putstatic` helper that may run a
    /// `<clinit>`) followed by a kept post-call exit site
    /// ([`Self::post_call_exit_sites`]), ascending. A frame suspended in such a
    /// call can leave for the interpreter the moment the call returns, which
    /// is the only door stage 3 may write a compiled frame's locals through
    /// ([`Self::debugger_exit_bci_after_call`]). Empty for every other compile.
    /// Wave 122: `(call bci, exit bci)` pairs, sorted, the exit bci being where
    /// the site's `OsrExit` map stands ([`Self::debugger_write_door`]).
    pub debugger_exit_call_bcis: Vec<(u32, u32)>,
    /// `(class, method, descriptor, class id)` this body was first published
    /// under, stamped by [`JitCache::put`] / [`JitCache::put_osr`] beside
    /// [`Self::owner_class_id`]. A superseded body keeps it, which is what lets
    /// a body only a baked caller still reaches be made not entrant.
    pub(crate) published_key: OnceLock<(Arc<str>, Arc<str>, Arc<str>, u32)>,
    /// Set once this body's entry has been patched to jump to its not-entrant
    /// stub ([`Self::make_not_entrant`]); the stub bakes this record's
    /// address, so it lives exactly as long as the code does.
    pub(crate) not_entrant: OnceLock<Box<NotEntrantRecord>>,
    /// Set once a class redefinition withdrew this body as stale
    /// ([`Self::is_withdrawn_by_redefinition`], interpreter round i1 wave 23,
    /// lane L6).
    pub(crate) withdrawn_by_redefinition: std::sync::atomic::AtomicBool,
    /// JIT round 21 wave 4 (lane codecache, CODECACHE21-2): live
    /// [`RetainedCode`] handles on this artifact, counted only when
    /// [`Self::retained_holder_count`] is set. The handle whose drop takes it
    /// to zero routes its `Arc` through the retirement queue, whatever
    /// `strong_count` reads, so two concurrent last-two drops can no longer
    /// both skip the queue.
    pub(crate) retained_holders: std::sync::atomic::AtomicUsize,
    /// `CRATONVM_JIT_RETAINED_HOLDER_COUNT` (default ON, `=0` restores the
    /// `strong_count > 1` shortcut), read once when the artifact is built so
    /// every handle of one artifact follows the same rule and the hot clone /
    /// drop path reads a field, not a flag.
    pub(crate) retained_holder_count: bool,
    /// Every deopt box of this optimizing body is a back-edge poll exit that
    /// tests [`SAFEPOINT_VERDICT_NAMED_BODY`] (`ir_lower`'s
    /// `IrPollModeExits::MethodEntryNamedOnly`), and there is at least one:
    /// no guard pad, no rethrow pad, no post-call exit. The VM's caller-held
    /// sync-direct binding admits such a body as if it had no box, because a
    /// body it binds is marked first ([`Self::mark_bound_by_a_caller_held_site`])
    /// and the VM never sets that bit for a marked body. Interpreter round i2
    /// wave 53, lane L2.
    pub named_poll_exits_only: bool,
    /// Some back-edge poll of this optimizing body leaves through a CALLER
    /// CHAIN: a loop inside a spliced callee (`ir_lower`'s
    /// `spliced_back_edge_mode_exit_state`, interpreter round i2 wave 57,
    /// lane L2). The VM's poll verdict then asks, before telling the frame to
    /// leave, whether every chain sink would rebuild the chain now
    /// (`vm::jit::helpers::chain_poll_exits_resumable_for`), as a post-call
    /// chain exit's verdict does. `false` for every other producer.
    pub chain_poll_exits: bool,
    /// Set (never cleared) once a compiled caller bound this body for a
    /// caller-held synchronized `CALL` ([`Self::mark_bound_by_a_caller_held_site`];
    /// interpreter round i2 wave 53, lane L2).
    pub(crate) bound_by_a_caller_held_site: std::sync::atomic::AtomicBool,
    /// Buffer offsets of the condition-code byte of every back-edge poll's
    /// flag-clear branch (`0F 84` inline, `0F 85` in the IR tier's outlined
    /// form) whose slow path may LEAVE this body for the interpreter: the
    /// polls a mode exit was admitted at, which name this body to the slow
    /// path. Recorded by both x86-64 tiers; empty on aarch64 and for a body
    /// with no such poll. [`Self::force_exit_polls`] rewrites exactly these
    /// bytes (interpreter round i1 wave 25, lane L6).
    pub(crate) exit_poll_sites: Vec<u32>,
    /// The flag byte those polls test (`JitRuntimeHelpers::safepoint_flag_addr`
    /// at compile time), which [`Self::force_exit_polls`] checks each site's
    /// `TEST` against before it writes.
    pub(crate) exit_poll_flag: usize,
    /// `(site, stub)` buffer offsets of every patchable post-call exit of this
    /// body (interpreter round i1 wave 27, lane L6): `site` is a five-byte
    /// `NOP` ([`not_entrant::ENTRY_PATCH_PAD`]) right after a real call,
    /// placed so that it lies inside one 8-aligned word; `stub` is the
    /// out-of-line `CALL` that asks the safepoint slow path for this body's
    /// verdict ([`POST_CALL_EXIT_VERDICT_ONLY`]) and leaves through an
    /// `OsrExit` map at the call's SUCCESSOR bci on a non-zero one.
    /// [`JitCache::force_withdrawn_exit_polls`] rewrites each site into
    /// `JMP rel32` to its stub, with [`Self::exit_poll_sites`], so a frame
    /// whose thread was inside the call when a class redefinition withdrew
    /// this body leaves for the interpreter as soon as the call returns
    /// instead of at its next back edge. Recorded by the single-pass tier's
    /// OSR compiles only; empty elsewhere.
    pub(crate) post_call_exit_sites: Vec<(u32, u32)>,
    /// How many of [`Self::post_call_exit_sites`] sit BEFORE a constant-pool
    /// helper call rather than after a call (single-pass tier,
    /// `x64/op_invoke.rs::emit_cp_helper_exit_site`; interpreter round i1
    /// wave 44, lane L2). Diagnostic only: the `exit polls candidate` line
    /// prints it (`cp-helper-sites=`).
    pub(crate) cp_helper_exit_sites: u32,
    /// Set when a redefinition of this body's OWN class spared its exits
    /// (`JitCache::force_withdrawn_exit_polls`): the body is that class's
    /// obsolete activation from then on, which the VM never lets leave, so
    /// no later redefinition forces it either (interpreter round i1 wave 27,
    /// lane L6).
    pub(crate) exits_spared_as_obsolete: std::sync::atomic::AtomicBool,
    /// Set when a redefinition of this OSR body's OWN class renumbered its
    /// constant pool and `JitCache::force_withdrawn_exit_polls_after` forced
    /// the body instead of sparing it (interpreter round i1 wave 45, lane L2;
    /// `interpreter-L2-proposal-an-own-class-compiled-activation-leaves-at-its-constant-pool-sites-FIXED-20261011`,
    /// behind `not_entrant::OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`): the VM's
    /// verdict then lets the obsolete activation leave for the interpreter at
    /// its next exit ([`Self::leaves_as_renumbered_obsolete`]). Set before the
    /// force pass writes the first site.
    pub(crate) leaves_as_renumbered_obsolete: std::sync::atomic::AtomicBool,
    /// The VM's answer, for a METHOD-ENTRY body marked
    /// [`Self::leaves_as_renumbered_obsolete`], to "can a stash sink rebuild
    /// its frame in the bytecode it was compiled from, moved onto the class's
    /// current pool?" (interpreter round i1 wave 46, lane L2;
    /// `not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`): 0 not asked
    /// since the mark, 1 yes, 2 no. Asked once by the safepoint verdict, so a
    /// forced body whose translation is refused pays one class-manager read,
    /// not one per back edge. Cleared whenever the force pass marks the body.
    pub(crate) renumbered_entry_resumes: std::sync::atomic::AtomicU8,
    /// The VM's answer, for a METHOD-ENTRY body of a class redefined since
    /// it was compiled, to the same question under an interpreter-only
    /// request (interpreter round i1 wave 47, lane L2;
    /// `not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`), keyed by the
    /// process's redefinition count it was asked at: `count << 2 | 1` yes,
    /// `count << 2 | 2` no, `0` never asked. A later redefinition (the count
    /// moved) voids it, so it needs no clearing.
    pub(crate) obsolete_entry_resumes_memo: std::sync::atomic::AtomicU64,
    /// Set once [`Self::force_exit_polls`] rewrote [`Self::exit_poll_sites`]
    /// (and, since wave 27, [`Self::post_call_exit_sites`]).
    pub(crate) exit_polls_forced: std::sync::atomic::AtomicBool,
    /// Set once [`Self::force_exit_polls`] wrote EVERY one of
    /// [`Self::post_call_exit_sites`] (none refused; interpreter round i6 wave
    /// 123, lane L2). [`Self::exit_polls_forced`] is set for a body whose
    /// post-call sites were refused too, so it cannot tell a debugger write
    /// (`Self::debugger_force_verdict`) that the exit after its call will
    /// leave.
    pub(crate) post_call_exits_forced: std::sync::atomic::AtomicBool,
    /// JIT round 23 wave 1, lane L1 (THROWS22-1, the OSR profit exit;
    /// `CRATONVM_JIT_OSR_PROFIT_EXIT`): set (never cleared) once
    /// [`JitCache::arm_osr_profit_exit`] armed this SINGLE-PASS OSR body
    /// because an optimizing OSR body for every loop header of its method
    /// became enterable. Its back-edge exit polls are then forced, and the
    /// VM's poll verdict tells its frame to leave for the interpreter at the
    /// poll's header, so the interpreter's next back edge enters the
    /// optimizing body. The OSR door never re-enters an armed body (it
    /// recompiles a fresh single-pass body instead). Independent of
    /// [`Self::exit_polls_forced`], so a later redefinition still forces the
    /// post-call exit sites of an armed body.
    pub(crate) osr_profit_exit_armed: std::sync::atomic::AtomicBool,
    /// Set (never cleared) once [`JitCache::disarm_osr_profit_exit`] took the
    /// arm back because a profit verdict for this body could not send its
    /// frame out (item 1 of
    /// `docs/feature-designs/deopt-osr.md`); such a
    /// body is never armed again.
    pub(crate) osr_profit_exit_disarmed: std::sync::atomic::AtomicBool,
    /// STASH20-2 (JIT round 20 wave 19, lane yStash): direct `CALL rel32`
    /// sites of this body [`JitCache::rebind_stale_call_site`] re-pointed from
    /// a not-entrant callee body at its current one. Non-zero, a site's
    /// displacement no longer names the body a frame it returns to was entered
    /// through, so the stack walker's call decode fails closed for this caller
    /// ([`Self::has_rebound_call_sites`]).
    pub(crate) rebound_call_sites: std::sync::atomic::AtomicU32,
    /// JIT round 20 wave 20, lane rev16: the `(entry, artifact_id)` of every
    /// body a [`JitCache::rebind_stale_call_site`] pointed one of this body's
    /// call sites at. Those calls bake that body exactly as
    /// [`Self::_direct_callee_entries`] bakes its own direct callees, but are
    /// listed in neither, so the invalidation and redefinition closures read
    /// this too ([`Self::rebound_call_reaches`]): without it, a caller rebound
    /// at a body later invalidated stayed published and kept CALLing the
    /// withdrawn code (the stub's forward it replaced tested `retired`).
    /// Appended under the cache's `mutation` lock, never shrunk.
    pub(crate) rebound_callees: parking_lot::Mutex<Vec<(usize, u64)>>,
    /// The constant-pool stamp of this body's compilation
    /// ([`current_compile_cp_stamp`] when it was built: the class-redefinition
    /// count its compilation began at), `None` without a registered source.
    /// The generation of every `(holder, cp index)` site it baked
    /// ([`cp_holder_word`]); what [`JitCache::min_unpatched_cp_stamp`] keeps
    /// the redefinition histories down to for a body that could not be made
    /// not entrant (interpreter round i1 wave 25, lane L6).
    pub(crate) compile_cp_stamp: Option<u64>,
    /// The classes (ids, sorted) whose method bytecode the compilation that
    /// built this body copied into code -- an IR or nested single-pass
    /// splice, an elided empty constructor, an inherited method compiled
    /// under its receiver's key -- as far as the VM noted each copy
    /// ([`JitCache::note_bytecode_copied`]) while the compilation's scope was
    /// open ([`open_compile_epoch_witness`]). Empty for a body built outside
    /// one. A redefinition of such a class makes the body stale
    /// ([`Self::copied_bytecode_of`]; interpreter round i1 wave 25, lane L6,
    /// `i20-L1-proposal-per-body-copied-bytecode-dependencies`).
    pub(crate) copied_classes: Box<[u32]>,
    /// The compilation that built this body lost a copy record: a copy was
    /// noted while its scope was borrowed, so it went to the cache-wide set
    /// only and `copied_classes` may be missing it (JIT round 21 wave 25,
    /// lane initwitness; `note_copy_in_open_compile`). Read only by the
    /// by-id finalizable-subtype rule (`withdraws_for_finalizable_subtype`,
    /// opt-in `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID`), which then takes the
    /// body's `inlined_methods` name evidence as the old rule does.
    pub(crate) copy_record_lost: bool,
    /// The bytecode this body was compiled from, as the VM's compile door
    /// held it (padded code, exception table, `max_stack` / `max_locals`),
    /// stamped once by the VM at publication
    /// ([`Self::stamp_compiled_source`]); empty where a publish site does not
    /// stamp it. A deoptimization sink rebuilds a trapped frame of a body
    /// whose class was REDEFINED since its compilation in THIS bytecode and
    /// restamps it with [`Self::compile_cp_stamp`], so the obsolete
    /// activation continues in its own code, as JVMTI specifies, instead of
    /// re-running the method from entry in the new code (round 13 wave 3,
    /// lane replay2; proposal R13-1). Holds no reference back to any
    /// compiled body, so it adds no cycle.
    pub(crate) compiled_source: OnceLock<Arc<cratonvm_jit_api::CachedBytecodeMethod>>,
    /// A weak reference to the `Arc` this body was bound under at
    /// publication ([`Self::note_bound_arc`]), so a deopt trampoline that
    /// names the body through its compile id can stash the body itself beside
    /// the frame (interpreter round i2 wave 60, lane L2;
    /// `i59-L2-proposal-every-stash-names-its-trapping-body`). Weak: it adds
    /// no cycle and keeps nothing alive.
    pub(crate) bound_arc: OnceLock<std::sync::Weak<CompiledMethod>>,
}

/// Which inline-cache shape an [`IcHolderRef`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IcSlotKind {
    /// A [`JitMICSlot`] in its holder's `_jit_mic_slots`.
    Mic,
    /// A [`JitPICSlot`] (inline ways or hashed table) in its holder's
    /// `_jit_pic_slots`.
    Pic,
}

/// One inline-cache slot that published a callee body (round 11 wave 19, lane
/// lhm). Kept on the CALLEE ([`CompiledMethod::ic_holders`]) so that a
/// supersede can retire exactly the slots holding the displaced body, instead
/// of the O(bodies x slots) walk invalidation takes.
///
/// The holder is a `Weak` plus the slot's index in the holder's slot vector,
/// never a raw slot address: a holder body can be dropped before its callee,
/// and a raw address would then dangle. The slot vectors are not modified
/// after the holder is shared, so an index taken at binding stays valid for
/// as long as the `Weak` upgrades.
pub(crate) struct IcHolderRef {
    pub(crate) holder: std::sync::Weak<CompiledMethod>,
    pub(crate) index: u32,
    pub(crate) kind: IcSlotKind,
}

/// A slot's own `(holder, index)`, bound once at publication by
/// [`bind_inline_cache_holders`]. Unbound slots (tests, bodies never shared)
/// register nothing.
pub(crate) type IcSlotHolder = std::sync::OnceLock<(std::sync::Weak<CompiledMethod>, u32)>;

impl CompiledMethod {
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the locals holding
    /// the `synchronized` blocks this body holds at safepoint id `sp_id`,
    /// outermost first ([`Self::taken_monitor_slots`]); empty when the body
    /// published none for that id.
    pub fn taken_monitor_locals_at(&self, sp_id: u32) -> &[u16] {
        self.taken_monitor_slots
            .binary_search_by_key(&sp_id, |&(pc, _)| pc)
            .map_or(&[], |i| &self.taken_monitor_slots[i].1)
    }

    /// JIT round 19 proposal wave 8, lane gc8 (GC719-1): the frame words
    /// (positive RBP offsets) holding the `synchronized` blocks this
    /// OPTIMIZING-tier body holds at safepoint id `sp_id`, outermost first
    /// ([`Self::ir_monitor_homes`]); empty when the body published none for
    /// that id.
    pub fn ir_monitor_homes_at(&self, sp_id: u32) -> &[i32] {
        self.ir_monitor_homes
            .binary_search_by_key(&sp_id, |&(id, _)| id)
            .map_or(&[], |i| &self.ir_monitor_homes[i].1)
    }

    /// The `compile_cp_stamp` field: the constant-pool generation of the
    /// bytecode this body was compiled from. A deoptimization sink stamps the
    /// interpreter frame it rebuilds from a trapped activation with it, so a
    /// frame of a body its class's redefinition replaced reads its own
    /// constants (interpreter round i1 wave 28, lane L3;
    /// `obsolete_frames::stamp_frame_rebuilt_from_compiled_code` in the VM).
    pub fn compile_cp_stamp(&self) -> Option<u64> {
        self.compile_cp_stamp
    }

    /// Record `source` as the bytecode this body was compiled from
    /// ([`Self::compiled_source`]). The first stamp wins; `false` when one
    /// was already there. The VM stamps it at publication, from the template
    /// its compile door compiled (round 13 wave 3, lane replay2).
    pub fn stamp_compiled_source(&self, source: Arc<CachedBytecodeMethod>) -> bool {
        self.compiled_source.set(source).is_ok()
    }

    /// The bytecode this body was compiled from, when its publish site
    /// stamped it ([`Self::stamp_compiled_source`]).
    pub fn compiled_source(&self) -> Option<&Arc<CachedBytecodeMethod>> {
        self.compiled_source.get()
    }

    /// Record the `Arc` this body is published under, beside its compile-id
    /// binding (`bind_compile_id`'s three publish sites). The first wins.
    /// Interpreter round i2 wave 60, lane L2.
    pub(crate) fn note_bound_arc(self: &Arc<Self>) {
        let _ = self.bound_arc.set(Arc::downgrade(self));
    }

    /// The weak reference [`Self::note_bound_arc`] recorded, if any (a body
    /// built and bound by a unit test through a `Box` has none).
    pub(crate) fn bound_weak(&self) -> Option<std::sync::Weak<CompiledMethod>> {
        self.bound_arc.get().cloned()
    }

    /// Record that the slot `(holder, index, kind)` published this body.
    /// Idempotent per slot. Dead holders are pruned when the list doubles, so
    /// the list is bounded by the live slots that ever cached this body.
    pub(crate) fn note_ic_holder(
        &self,
        holder: &std::sync::Weak<CompiledMethod>,
        index: u32,
        kind: IcSlotKind,
    ) {
        let mut holders = self.ic_holders.lock();
        if holders
            .iter()
            .any(|h| h.index == index && h.kind == kind && h.holder.ptr_eq(holder))
        {
            return;
        }
        if holders.len() >= 32 && holders.len().is_power_of_two() {
            holders.retain(|h| h.holder.strong_count() > 0);
        }
        holders.push(IcHolderRef {
            holder: holder.clone(),
            index,
            kind,
        });
    }

    /// Retire every inline-cache slot that still publishes THIS body, which a
    /// later publication of the same key has just superseded (round 11 wave
    /// 19, lane lhm, `r11w18-hashmap-inline-caches-keep-a-superseded-callee`).
    ///
    /// A MIC keeps its receiver class and loses only its entry word (the shape
    /// `clear_compiled_entry` leaves after an invalidation), so the next call
    /// through it takes the resolving helper, which finds the current body and
    /// publishes it with `update`. A PIC way or hashed-table way holding this
    /// body is retired (guard first, then word), so the helper re-installs the
    /// receiver's current body. Every owner released goes through the
    /// retirement queue, exactly as in the invalidation walk.
    ///
    /// Sound for the same reason the invalidation walk is: a reader already
    /// past a guard loads the old word, whose body the deferred owner keeps
    /// mapped, or zero; and the superseded body is still correct code, so a
    /// call that races this walk and runs it once more is not wrong.
    ///
    /// Returns how many slots were cleared.
    pub(crate) fn retarget_superseded_inline_caches(&self) -> usize {
        let holders = std::mem::take(&mut *self.ic_holders.lock());
        if holders.is_empty() {
            return 0;
        }
        let entry = self.entry_ptr() as usize;
        let mut cleared = 0usize;
        for h in holders {
            let Some(holder) = h.holder.upgrade() else {
                continue;
            };
            let hit = match h.kind {
                IcSlotKind::Mic => holder
                    ._jit_mic_slots
                    .get(h.index as usize)
                    .is_some_and(|slot| slot.clear_compiled_entry_owned_by(self)),
                IcSlotKind::Pic => holder
                    ._jit_pic_slots
                    .get(h.index as usize)
                    .is_some_and(|slot| slot.retire_entry(entry)),
            };
            if hit {
                cleared += 1;
            }
            // The upgrade may be the last strong reference if the holder is
            // being withdrawn concurrently: release that one the way every
            // other owner is released. A holder something else still owns is
            // just dropped (queueing a live body would count its bytes as
            // retired and bump the retire generation for nothing).
            if Arc::strong_count(&holder) == 1 {
                defer_jit_owner(Some(holder));
            } else {
                drop(holder);
            }
        }
        cleared
    }
}

/// Bind every inline-cache slot of `body` to `(body, index)`, so a later
/// publication through the slot is recorded on its callee (round 11 wave 19,
/// lane lhm). Called where a body becomes reachable by generated code:
/// [`JitCache::put`], [`JitCache::put_osr`] and [`register_unpublished_body`].
///
/// A slot that already published before the binding (a seeded PIC way, a MIC
/// filled at build time) registers its current owners here, so it is not
/// missed. A second call is a no-op: the binding is write-once.
pub(crate) fn bind_inline_cache_holders(body: &Arc<CompiledMethod>) {
    for (i, slot) in body._jit_mic_slots.iter().enumerate() {
        let Ok(index) = u32::try_from(i) else {
            break;
        };
        if slot.holder.set((Arc::downgrade(body), index)).is_ok() {
            slot.register_current_owner();
        }
    }
    for (i, slot) in body._jit_pic_slots.iter().enumerate() {
        let Ok(index) = u32::try_from(i) else {
            break;
        };
        if slot.holder.set((Arc::downgrade(body), index)).is_ok() {
            slot.register_current_owners();
        }
    }
}

// SAFETY: the raw pointers a `CompiledMethod` holds address its own boxed arenas
// (strings, invoke infos, inline-cache slots, deopt boxes) and its own
// `ExecutableBuffer`, all of which move with it; nothing is tied to the thread
// that compiled it.
unsafe impl Send for CompiledMethod {}
// SAFETY: state that changes after publication is atomic (`retired`, the
// inline-cache slot words, counters) or guarded by its own lock; everything else
// is written only before the artifact is published.
unsafe impl Sync for CompiledMethod {}

impl Drop for CompiledMethod {
    fn drop(&mut self) {
        // Before any field drops: `_jit_invoke_infos`' boxes are freed after
        // this function returns, and their addresses are the site keys of the
        // helpers' positive dispatch memos. The retire bump must come before
        // the allocator can re-issue one. See `JIT_CACHE_RETIRE_SHIFT`. A
        // judged `VarHandle` record's `info` is such a key too (its helper's
        // cold arm dispatches with it; round 15 wave 6, lane vh8).
        if !self._jit_invoke_infos.is_empty() || !self._jit_vh_judged_sites.is_empty() {
            JIT_CACHE_GENERATION
                .fetch_add(JIT_CACHE_RETIRE_UNIT, std::sync::atomic::Ordering::Release);
        }
        let entry = self.entry as usize;
        if dbg_stale_ic_enabled() {
            report_stale_ic_holders(entry);
        }
        if dbg_jit_code_free_enabled() {
            // Round 14 wave 1 (lane codecache, `gcd-d10v`): name the body, so a
            // crash report's freed `base=` can be matched to a method. Printed
            // when the last owner goes; the unmap (`[jit-code-free]`, same flag)
            // follows now or, for a rescued mapping, when the queue releases it.
            eprintln!(
                "[jit-code-free-name] entry={entry:#x} artifact={} method={}",
                self.artifact_id,
                self.method_key_for_report()
                    .unwrap_or_else(|| "?".to_string()),
            );
        }
        unregister_jit_code_range(entry);
        // Withdraw the identity binding alongside the range. No frame of a
        // dropped method can be live, so a mirror still holding this id
        // resolves to `None` afterwards — returning the scan to its old decode
        // path rather than handing it a freed artifact.
        release_compile_id(self.compile_id);
        // Same withdrawal, same reason, and it must happen here too: the buffer
        // this artifact owns is unmapped as soon as this function returns, and
        // the address is then reusable by the next `alloc_executable`.
        unregister_jit_method_name(entry);
        // The implicit-null withdrawal is deliberately NOT here. It used to be,
        // with the same sentence as the two above, and it was wrong in a way the
        // other two are not: those cost a stale diagnostic name, this one is the
        // difference between a `NullPointerException` and a process-killing
        // fault. Retiring the range when the last owner goes away is too EARLY —
        // `ExecutableBuffer::drop`'s rescue arm exists precisely because a
        // published buffer can reach `drop` while a thread is still executing
        // inside it, and the rescue keeps the mapping alive in the retirement
        // queue. A body rescued that way would have been left running with its
        // recovery table already tombstoned, so a null receiver at one of its
        // elided sites faulted unrecoverably.
        //
        // `ExecutableBuffer::drop` owns this withdrawal now and does it at the
        // one load-bearing instant: immediately before `free_executable`, when
        // the ADDRESS becomes reusable. Every unmap reaches it, the rescued one
        // included — the retirement queue holds the orphan as
        // `RetiredCode::Mapping` and drops it under an `AuthorisedReclaim`, which
        // runs that same `Drop`. It retires the whole `capacity` rather than this
        // method's `pos()`, so it is also a superset of what this line retired.
        if let Some(owners) = JIT_ENTRY_OWNERS.get() {
            let mut owners = owners.lock();
            if owners
                .get(&entry)
                .is_some_and(|owner| owner.strong_count() == 0)
            {
                owners.remove(&entry);
            }
        }

        // Purge cached OSR trampolines before the body mapping is returned.
        #[cfg(target_arch = "x86_64")]
        {
            let start = entry;
            let end = start.saturating_add(self._buffer.pos());
            let mut cache = osr_trampoline_cache().lock();
            cache.retain(|&(target, _), _| !(target >= start && target < end));
        }

        // JIT round 21 wave 4 (lane codecache, CODECACHE21-1): the callees
        // this body's inline caches and cells keep mapped leave through the
        // queue, never as a cascade of this body's own release.
        self.withdraw_inline_cache_owners();

        // JIT round 21 wave 4 (lane codecache): the `_buffer` field drop that
        // follows this body will RESCUE the mapping into the retirement queue
        // (a cascaded or stray last-owner release, where a thread may still be
        // inside it). Its code still dereferences the boxes below by baked
        // address, so they wait in the queue under the same buffer start
        // instead of being freed with the fields. See `RescuedSideTables`.
        if self._buffer.release_would_be_rescued() {
            // Cast: the buffer start, the identity the queue's proof is about.
            let buffer_start = self._buffer.as_ptr() as usize;
            if dbg_jit_code_free_enabled() {
                eprintln!(
                    "[jit-rescued-side-tables] entry={entry:#x} artifact={} kept with the \
rescued mapping at base={buffer_start:#x}",
                    self.artifact_id,
                );
            }
            let tables = RescuedSideTables::take_from(self);
            enqueue_retired(RetiredCode::SideTables(Box::new(tables)), buffer_start, 0);
        }
    }
}

/// ABI words `CompiledMethod::try_call*` pass through the padded sixteen-word
/// thunk, context included (round 12 wave 2, lane calls; the helper-side twin
/// is `vm/src/jit/helpers.rs::try_call_compiled_entry_wide`).
pub const TRY_CALL_MAX_WORDS: usize = 16;

/// A compiled-entry call of 9..=[`TRY_CALL_MAX_WORDS`] words, x86-64 only:
/// the words past `args` are zero and never read (caller-cleanup C ABI; every
/// x86-64 prologue reads exactly the stack words it declares).
///
/// # Safety
/// `entry` is a validated compiled entry taking `ctx.is_some() + args.len()`
/// words, and `ctx.is_some() + args.len() <= TRY_CALL_MAX_WORDS`.
pub(crate) unsafe fn call_entry_padded16(entry: *const u8, ctx: Option<i64>, args: &[i64]) -> i64 {
    let mut w = [0i64; TRY_CALL_MAX_WORDS];
    let lead = usize::from(ctx.is_some());
    if let Some(ctx) = ctx {
        w[0] = ctx;
    }
    for (slot, &arg) in w[lead..].iter_mut().zip(args) {
        *slot = arg;
    }
    type Wide = unsafe extern "C" fn(
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) -> i64;
    let f: Wide = std::mem::transmute(entry);
    f(
        w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7], w[8], w[9], w[10], w[11], w[12], w[13],
        w[14], w[15],
    )
}

impl CompiledMethod {
    /// The class a live frame of this body reports: [`Self::holder_class_id`]
    /// when the publishing door stamped one, else [`Self::owner_class_id`].
    pub fn frame_class_id(&self) -> u32 {
        if self.holder_class_id != cratonvm_types::jit_activation::NO_OWNER_CLASS
            && self.holder_class_id != 0
        {
            self.holder_class_id
        } else {
            self.owner_class_id
        }
    }

    /// [`Self::method_label`], shared. Minted once per artifact on first call,
    /// so it must not be called before the label is final -- every writer sets
    /// it at compile time, before the artifact is published, and every reader
    /// (a stack walk) sees only published artifacts.
    pub fn method_label_shared(&self) -> std::sync::Arc<str> {
        std::sync::Arc::clone(
            self.method_label_arc
                .get_or_init(|| std::sync::Arc::from(self.method_label.as_str())),
        )
    }

    /// deopt-osr Step 9 follow-up (a) — stamp this artifact's retained
    /// [`crate::deopt::DeoptEpochGuard`] (baked into every frame-deopt stub) with
    /// its creation epoch and a stable pointer to the method's live
    /// compilation-epoch cell. Called once by the VM at install, under
    /// `deopt_real_enabled()`. No-op when no guard was emitted (production
    /// artifacts: `deopt_epoch_guard` is null) so it is gate-off byte-identical.
    ///
    /// Nothing on the trap path reads the stamp any more: `x64_deopt_entry`
    /// accepts the guard argument and ignores it, because the
    /// `CRATONVM_JIT_FREE_CODE=1` mode whose freed-point short-circuit it fed
    /// was deleted, and an executing artifact always owns the points its stubs
    /// name (see that function's body). The stamp is kept for the stub ABI and
    /// for diagnostics.
    ///
    /// # Safety
    /// `live_epoch_cell` must be null or a stable, process-lifetime `AtomicU64`
    /// address (e.g. one handed out by `SharedVm::method_epochs`).
    pub fn stamp_deopt_epoch_guard(
        &self,
        creation_epoch: u64,
        live_epoch_cell: *const std::sync::atomic::AtomicU64,
    ) {
        if self.deopt_epoch_guard.is_null() {
            return;
        }
        use std::sync::atomic::Ordering;
        // SAFETY: a non-null `deopt_epoch_guard` is the leaked, retained guard
        // baked by `emit_deopt_stubs` (valid for the process lifetime).
        let guard = unsafe { &*self.deopt_epoch_guard };
        guard
            .creation_epoch
            .store(creation_epoch, Ordering::Relaxed);
        guard.live_epoch_cell.store(
            live_epoch_cell as *mut std::sync::atomic::AtomicU64,
            Ordering::Release,
        );
    }

    /// Create from a completed executable buffer (pure method, no context needed).
    ///
    /// Raw machine-code bytes of this compiled method (for diagnostics /
    /// disassembly). The slice is the full executable buffer.
    pub fn code_bytes(&self) -> &[u8] {
        self._buffer.as_slice()
    }

    /// Finalizes the buffer (transitions from writable to executable).
    ///
    /// ABORTS the process if the OS refuses that flip — see
    /// [`Self::try_new`], the form a compile path that can decline to the
    /// interpreter should use.
    pub fn new(mut buffer: ExecutableBuffer) -> Self {
        buffer.trim_unused_tail();
        buffer.finalize();
        Self::from_finalized_buffer(buffer)
    }

    /// [`Self::new`] that returns `None` instead of aborting when the RW->RX
    /// flip is refused (round 9 wave 2;
    /// `executable-buffer-finalize-aborts-the-process-on-protect-failure-20260918.md`).
    ///
    /// The refused buffer is dropped here: it is still RW and was never
    /// published, so that is an ordinary unmap. The failure is counted
    /// ([`code_buffer_protect_failures`]) and the first one is reported, so a
    /// process whose compiles are all declining for this reason says why.
    pub fn try_new(mut buffer: ExecutableBuffer) -> Option<Self> {
        buffer.trim_unused_tail();
        if let Err(e) = buffer.try_finalize() {
            note_code_buffer_protect_failure(&e);
            return None;
        }
        Some(Self::from_finalized_buffer(buffer))
    }

    /// [`Self::new_with_context`] that returns `None` instead of aborting when
    /// the RW->RX flip is refused. See [`Self::try_new`].
    pub fn try_new_with_context(buffer: ExecutableBuffer) -> Option<Self> {
        let mut compiled = Self::try_new(buffer)?;
        compiled.needs_context = true;
        Some(compiled)
    }

    /// The constructor body shared by every `new*` form. `buffer` must already
    /// be finalized (RX).
    pub(crate) fn from_finalized_buffer(buffer: ExecutableBuffer) -> Self {
        let entry = buffer.as_ptr();
        let entry_validated_once = Self::validate_entry_once(entry);
        Self {
            _buffer: buffer,
            entry,
            needs_context: false,
            entry_validated_once,
            _jit_strings: Vec::new(),
            _jit_local_handler_sites: Vec::new(),
            _jit_invoke_infos: Vec::new(),
            _jit_vh_judged_sites: Vec::new(),
            _jit_entry_counter: None,
            _jit_c1_branch_counters: None,
            _jit_mic_slots: Vec::new(),
            _jit_pic_slots: Vec::new(),
            _jit_cycle_cells: Vec::new(),
            _jit_typecheck_seen_cells: Vec::new(),
            _direct_callee_entries: Vec::new(),
            _direct_callee_expected: Vec::new(),
            _direct_callee_roots: Vec::new(),
            osr_pc_to_native: None,
            osr_num_locals: 0,
            osr_num_reg_locals: 0,
            osr_local_assignments: None,
            osr_dead_mask: None,
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_callee_saved_regs: None,
            ir_saved_gpr_slots: None,
            osr_callee_saved_xmms: None,
            osr_xmm_saved_base: 0,
            frame_layout: FrameLayout::default(),
            method_label: String::new(),
            method_label_arc: std::sync::OnceLock::new(),
            safepoint_id_index: std::sync::OnceLock::new(),
            shadow_savebase_slot_off: 0,
            shadow_frame_block: false,
            osr_heap_local_offset: 0,
            jit_thread_slot_off: 0,
            stack_floor_slot_off: 0,
            osr_frame_record: 0,
            shadow_thread_slot_off: 0,
            shadow_savetop_slot_off: 0,
            shadow_off_in_thread: 0,
            has_dispatch: false,
            used_ir_backend: false,
            inlined_methods: Vec::new(),
            inline_tally: InlineDecisionTally::default(),
            // Two empty `Vec`s. No allocation, and none unless this compile
            // actually splices something.
            inline_frame_map: Default::default(),
            npe_trap_map: Default::default(),
            safepoint_bci_table: Vec::new(),
            deopt_points: Vec::new(),
            ir_osr_entries: Vec::new(),
            ir_osr_entry_kinds: Vec::new(),
            ir_osr_opaque_entry_bcis: Box::new([]),
            ir_osr_sentinel_free: false,
            ir_osr_exception_exit_bcis: None,
            ir_osr_guard_exit_bcis: None,
            ir_osr_guard_fact_needs_opaque_entries: false,
            _deopt_point_boxes: Vec::new(),
            _forced_chain_deopt_counters: Vec::new(),
            _forced_chain_deopt_points: Vec::new(),
            _poll_stress_cells: Vec::new(),
            trap_cause_points: Box::new([]),
            _jit_class_init_cells: Vec::new(),
            oop_maps: Vec::new(),
            staged_args_unmapped_sp_ids: Vec::new(),
            frame_liveness: FrameLivenessTable::default(),
            // Start unverified: the production codegen path moves a
            // fully-built vector into `oop_maps` wholesale (bypassing
            // `push_oop_map`), so the first `find_oop_map_for_pc` call
            // must verify/sort once. `push_oop_map` keeps the flag
            // precise for the incremental-build path.
            compiled_via_osr: false,
            osr_compiled_entry_pc: None,
            static_init_classes: Vec::new(),
            static_inits_done: std::sync::atomic::AtomicBool::new(false),
            oop_maps_sorted: false,
            sp_id_slot_off: 0,
            // Every existing producer keeps the full frame contract.
            frame_holds_no_references: false,
            // No producer but the optimizing tier, under its opt-in switch.
            ir_prim_colour_claim: Box::new([]),
            ir_prim_colour_clear_bytes: (0, 0),
            ir_frame_anatomy: String::new(),
            return_site_bci_table: Vec::new(),
            fully_oop_covered: false,
            fully_shadow_covered: false,
            // deopt-osr scaffolding: default to the safe re-run path; no
            // emitter sets these yet (see docs/feature-designs/deopt-osr.md).
            can_deopt_resume: false,
            vthread_boundary_guards: false,
            // Vacuously true: an artifact that spliced nothing has no
            // relocated body that could commit anything.
            spliced_bodies_side_effect_free: true,
            splice_scope_class_ids: Vec::new(),
            splice_scope_sources: Vec::new(),
            ir_guarded_splice_bcis: Box::new([]),
            sp_nested_miss_cells: Box::new([]),
            sp_nested_withdraw_claimed: std::sync::atomic::AtomicBool::new(false),
            ir_guarded_splice_classes: Box::new([]),
            splice_expected_throw_trips: std::sync::atomic::AtomicU32::new(0),
            can_osr_exit: false,
            has_indy_trap: false,
            has_frameless_trap_stub: false,
            frameless_traps_replay_exact: false,
            osr_exit_policy_memo: std::sync::OnceLock::new(),
            osr_exit_points: Vec::new(),
            post_call_own_exit_bcis: Vec::new(),
            compilation_epoch: 0,
            deopt_epoch_guard: std::ptr::null(),
            requires_wrapped_entry: false,
            self_locks_monitor: false,
            self_lock_hold: None,
            taken_monitor_slots: Box::default(),
            ir_monitor_homes: Box::default(),
            keep_live_optimizing_body: false,
            owner_class_id: cratonvm_types::jit_activation::NO_OWNER_CLASS,
            holder_class_id: cratonvm_types::jit_activation::NO_OWNER_CLASS,
            compile_id: 0,
            install_epoch: current_compile_install_epoch(),
            invalidation_epoch: current_compile_invalidation_epoch(),
            artifact_id: next_artifact_id(),
            retired: std::sync::atomic::AtomicBool::new(false),
            superseded: std::sync::atomic::AtomicBool::new(false),
            ic_holders: parking_lot::Mutex::new(Vec::new()),
            entry_java_args: u16::MAX,
            method_declares_no_handlers: false,
            service_arg_copies: Vec::new(),
            rerun_window_closed_sp_ids: Vec::new(),
            service_arg_resume_only_sp_ids: Vec::new(),
            reentry_only: false,
            debugger_home_locals: Vec::new(),
            debugger_exit_call_bcis: Vec::new(),
            published_key: OnceLock::new(),
            not_entrant: OnceLock::new(),
            withdrawn_by_redefinition: std::sync::atomic::AtomicBool::new(false),
            retained_holders: std::sync::atomic::AtomicUsize::new(0),
            retained_holder_count: retained_holder_count_enabled(),
            named_poll_exits_only: false,
            chain_poll_exits: false,
            bound_by_a_caller_held_site: std::sync::atomic::AtomicBool::new(false),
            exit_poll_sites: Vec::new(),
            exit_poll_flag: 0,
            post_call_exit_sites: Vec::new(),
            cp_helper_exit_sites: 0,
            exits_spared_as_obsolete: std::sync::atomic::AtomicBool::new(false),
            leaves_as_renumbered_obsolete: std::sync::atomic::AtomicBool::new(false),
            renumbered_entry_resumes: std::sync::atomic::AtomicU8::new(0),
            obsolete_entry_resumes_memo: std::sync::atomic::AtomicU64::new(0),
            exit_polls_forced: std::sync::atomic::AtomicBool::new(false),
            post_call_exits_forced: std::sync::atomic::AtomicBool::new(false),
            osr_profit_exit_armed: std::sync::atomic::AtomicBool::new(false),
            osr_profit_exit_disarmed: std::sync::atomic::AtomicBool::new(false),
            rebound_call_sites: std::sync::atomic::AtomicU32::new(0),
            rebound_callees: parking_lot::Mutex::new(Vec::new()),
            compile_cp_stamp: current_compile_cp_stamp(),
            copied_classes: current_compile_copied_classes(),
            copy_record_lost: current_compile_copy_record_lost(),
            compiled_source: OnceLock::new(),
            bound_arc: OnceLock::new(),
        }
    }

    /// Create from a completed executable buffer (needs VM context pointer).
    ///
    /// Finalizes the buffer (transitions from writable to executable).
    pub fn new_with_context(buffer: ExecutableBuffer) -> Self {
        // The two constructors differed in exactly one field and nothing else;
        // two 80-line literals kept in step by hand is how a new field gets
        // initialised in one and forgotten in the other. Delegating keeps the
        // `finalize`, the epoch stamps and the artifact id in ONE place.
        let mut compiled = Self::new(buffer);
        compiled.needs_context = true;
        compiled
    }

    /// NEW-12: append a precise oop map entry for a safepoint.
    ///
    /// Called by the JIT code emitter at every point where GC can be
    /// triggered (new, newarray, anewarray, invoke of a callee that
    /// may allocate). The `native_pc_offset` must be the offset of the
    /// instruction that follows the safepoint call; at GC time the
    /// walker matches on this offset directly (no PC-range search).
    ///
    /// The caller is responsible for ordering entries by
    /// `native_pc_offset` when pushing; otherwise
    /// [`Self::find_oop_map_for_pc`] will sort them lazily on first
    /// lookup. Direct append is the common path because the compiler
    /// walks the method in bytecode order.
    pub fn push_oop_map(&mut self, entry: OopMapEntry) {
        // A push invalidates any cached "sorted" knowledge from a prior
        // `find_oop_map_for_pc` call, so clear the flag; the next lookup
        // re-verifies (and sorts only if actually needed). This keeps the
        // O(n) sortedness scan off the per-safepoint GC lookup path while
        // remaining correct regardless of push order.
        self.oop_maps_sorted = false;
        // And any safepoint-id index (`has_oop_map_for_safepoint_id`).
        self.safepoint_id_index.take();
        self.oop_maps.push(entry);
    }

    /// NEW-12: locate the oop map for a specific native PC offset.
    ///
    /// Returns `Some(&OopMapEntry)` if the compiled method has a map
    /// exactly at `native_pc_offset`, or `None` otherwise. The GC
    /// walker calls this with the current frame's return PC minus
    /// the entry pointer. When it returns `None`, the conservative
    /// scan is used for that frame as a fallback.
    ///
    /// Sorts the `oop_maps` vector on first call so subsequent
    /// lookups are O(log n) binary searches. Idempotent: the sort is
    /// cheap and runs only if the vector is out of order.
    pub fn find_oop_map_for_pc(&mut self, native_pc_offset: u32) -> Option<&OopMapEntry> {
        // Ensure sorted for binary search. The O(n) `windows(2)`
        // sortedness check (and any sort) runs at most once per compiled
        // method: `oop_maps_sorted` caches the result so subsequent
        // GC-time lookups skip the scan entirely. `push_oop_map` clears
        // the flag, so a lookup after any push re-verifies. (The flag
        // starts `false` to cover the wholesale
        // `cm.oop_maps = compiler.oop_maps` codegen path, which bypasses
        // `push_oop_map`.)
        if !self.oop_maps_sorted {
            let already_sorted = self
                .oop_maps
                .windows(2)
                .all(|w| w[0].native_pc_offset <= w[1].native_pc_offset);
            if !already_sorted {
                self.oop_maps.sort_by_key(|e| e.native_pc_offset);
            }
            self.oop_maps_sorted = true;
        }
        debug_assert!(
            self.oop_maps
                .windows(2)
                .all(|w| w[0].native_pc_offset <= w[1].native_pc_offset),
            "oop_maps must be sorted by native_pc_offset before binary search",
        );
        match self
            .oop_maps
            .binary_search_by_key(&native_pc_offset, |e| e.native_pc_offset)
        {
            Ok(idx) => Some(&self.oop_maps[idx]),
            Err(_) => None,
        }
    }

    /// NEW-12: whether this method has at least one precise oop map.
    ///
    /// Used by `JitEntryGuard::enter_with_compiled` to decide whether
    /// the frame can be walked precisely. A method with no oop maps
    /// falls back to the conservative scan for its active frame.
    pub fn has_precise_oop_maps(&self) -> bool {
        !self.oop_maps.is_empty()
    }

    /// Locate the map selected by the safepoint id stored in a live JIT frame.
    ///
    /// The x64 emitter records the bytecode PC of each GC-capable safepoint in
    /// a dedicated frame slot immediately before its call. Unlike
    /// [`Self::find_oop_map_for_pc`], this lookup is intentionally keyed by
    /// bytecode PC: while a helper is active, the native return address belongs
    /// to the helper, but the frame slot remains stable and identifies the
    /// caller's exact safepoint map.
    #[inline]
    pub fn find_oop_map_for_safepoint_id(&self, bytecode_pc: u32) -> Option<&OopMapEntry> {
        self.oop_maps
            .iter()
            .find(|map| map.bytecode_pc == bytecode_pc)
    }

    /// `self.find_oop_map_for_safepoint_id(id).is_some()`, by a binary search
    /// of a sorted index of the maps' ids instead of a linear scan of the maps
    /// (JIT round 16 wave 15, lane throwfill).
    ///
    /// A stack walk asks this for every compiled activation it places by its
    /// safepoint id (`activation_bci` in the VM), and an [`OopMapEntry`] is
    /// over a hundred bytes: the scan read two cache lines per safepoint of
    /// the method, before the one that matched, per frame per throwable. The
    /// index is built once per artifact, on first use (a published artifact's
    /// maps never change), and is the distinct ids of the maps it was built
    /// from. Exactly the scan's answer: an index built from a different number
    /// of maps than the artifact holds now (a map pushed after it was built,
    /// before publication) is not trusted, and the scan answers instead;
    /// [`Self::push_oop_map`] also drops it.
    pub fn has_oop_map_for_safepoint_id(&self, bytecode_pc: u32) -> bool {
        let (built_from, ids) = self.safepoint_id_index.get_or_init(|| {
            let mut ids: Vec<u32> = self.oop_maps.iter().map(|map| map.bytecode_pc).collect();
            ids.sort_unstable();
            ids.dedup();
            (self.oop_maps.len(), ids.into_boxed_slice())
        });
        if *built_from != self.oop_maps.len() {
            return self.find_oop_map_for_safepoint_id(bytecode_pc).is_some();
        }
        ids.binary_search(&bytecode_pc).is_ok()
    }

    /// The bytecode index an OPTIMIZING-tier safepoint id names, if this
    /// artifact recorded one. See [`Self::safepoint_bci_table`].
    ///
    /// `None` for every single-pass artifact (the table is empty there, and the
    /// id IS the bci) and for an id the lowerer emitted no translation for. A
    /// caller must treat `None` as "no line available", never as bci 0.
    #[inline]
    pub fn safepoint_bci(&self, safepoint_id: u32) -> Option<u32> {
        self.safepoint_bci_table
            .binary_search_by_key(&safepoint_id, |(id, _)| *id)
            .ok()
            .map(|i| self.safepoint_bci_table[i].1)
    }

    /// Unified per-site record lookup shared by GC root scanning, deopt, and
    /// stack tracing / `Throwable`.
    pub fn site_record(
        &self,
        native_offset: Option<u32>,
        safepoint_id: Option<u32>,
        bci_hint: Option<u32>,
    ) -> CompiledSiteRecord<'_> {
        let valid_sp_id = safepoint_id.filter(|&id| self.safepoint_bci(id).is_some());
        let bci = valid_sp_id
            .and_then(|id| self.safepoint_bci(id))
            .or(bci_hint);
        let oop_map = valid_sp_id
            .and_then(|id| self.oop_maps.iter().find(|m| m.bytecode_pc == id));
        let (inline_chain, inline_chain_exact) = if self.inline_frame_map.is_empty() {
            (None, false)
        } else if let Some(off) = native_offset {
            (
                self.inline_frame_map.chain_for_native_offset_shared(off),
                self.inline_frame_map.chain_for_native_offset_shared(off).is_some(),
            )
        } else if let Some(chain_opt) = valid_sp_id
            .filter(|_| self.used_ir_backend)
            .and_then(|id| self.inline_frame_map.chain_for_safepoint_id_shared(id))
        {
            (chain_opt, false)
        } else if let Some(chain) =
            bci.and_then(|b| self.inline_frame_map.chain_for_safepoint_bci_shared(b))
        {
            (Some(chain), false)
        } else {
            (None, false)
        };
        CompiledSiteRecord {
            safepoint_id: valid_sp_id,
            bci,
            oop_map,
            inline_chain,
            inline_chain_exact,
        }
    }

    // `CompiledMethod::find_deopt_point` was DELETED in round 10 (wave 9, lane
    // `deoptretire`). It was an exact-offset binary search over `deopt_points`
    // with no production caller: both backends BAKE the address of the
    // `deopt_points` element into the stub that needs it
    // (`x64/deopt_stubs.rs::resolve_baked_point`, `ir_lower`'s `deopt_boxes`),
    // so the runtime already holds the point and never looks one up by offset.
    //
    // Do not re-add it. `native_offset` is **not a unique key** for
    // `deopt_points`: `build_and_record_deopt_point` emits no machine code and
    // the three recorder families that call it keep their idempotence in three
    // maps that do not see each other (`deopt_box_ptr_by_bci`,
    // `osr_exit_box_ptr_by_bci`, `exc_frame_box_ptr_by_bci`), so two adjacent
    // metadata-only records at one pc land on one offset and a search would
    // return an arbitrary one of them. `DeoptMetadataError::DeoptPointsUnsorted`
    // is deliberately `>` rather than `>=` for the same reason, and the
    // sortedness the deleted search relied on is checked there — on the install
    // path, in release builds — rather than by a `debug_assert`.
    //
    // If a future consumer genuinely needs an offset-keyed lookup (a deopt
    // arriving at a pc with no baked pointer, e.g. a signal-delivered trap), the
    // order of work is: make the recorders agree on one identity, then make the
    // verifier enforce it, then write the lookup. Not the other way round.
    // `docs/feature-designs/deopt-osr.md`
    // is the record.

    /// Whether this method requires a SharedVm pointer as its hidden first argument.
    pub fn needs_context(&self) -> bool {
        self.needs_context
    }

    /// Return the raw entry point pointer for direct calls from JIT code.
    /// Make this artifact the owner of the callees its baked direct calls reach.
    ///
    /// For a body that is never published through [`JitCache`] (the accepted
    /// optimizing OSR body), `prepare_for_publication` never roots its callees;
    /// the compile door's pins were kept by a MEMO entry instead, and a thread
    /// that re-entered the body through a memo hit holds the artifact but not
    /// the memo's pins. Purging the memo entry then freed a callee under a
    /// frame parked inside the body, which returned into a baked `call` to the
    /// unmapped page (`ForkJoinPool.signalWork` under a parked worker). The
    /// pins live exactly as long as the artifact once adopted.
    pub fn adopt_callee_pins(&mut self, pins: Vec<RetainedCode>) {
        self._direct_callee_roots.extend(pins);
    }

    pub fn entry_ptr(&self) -> *const u8 {
        // See the `entry` field: the freed-buffer `Dead` answer relies on it.
        debug_assert_eq!(
            self.entry,
            self._buffer.as_ptr(),
            "an entry is its buffer's base"
        );
        self.entry
    }

    /// Stage 5 (precise oop maps) — length in bytes of this method's emitted
    /// machine code, so the GC code-range registry can record `[entry,
    /// entry+code_len())` for return-address → CompiledMethod resolution while
    /// walking the JIT RBP chain.
    pub fn code_len(&self) -> usize {
        self._buffer.pos()
    }

    /// zt pl4 item 1: the code-cache bytes this body occupies (its buffer's
    /// capacity), the perf-data `sun.ci.nmethodSize` figure.
    pub fn code_capacity(&self) -> usize {
        self._buffer.capacity()
    }

    /// Debug-only: raw emitted machine code bytes.
    pub fn _buffer_slice_for_debug(&self) -> &[u8] {
        self._buffer.as_slice()
    }

    /// Backward-compatible alias for `needs_context()`.
    pub fn needs_heap(&self) -> bool {
        self.needs_context
    }

    /// How many times this artifact's method-entry prologue has run.
    ///
    /// `0` when the counter was never armed (`CRATONVM_JIT_ENTRY_COUNTER=0`,
    /// the kill switch; it is on by default since round 9 wave 3) — the same answer a genuinely un-entered body
    /// gives, and deliberately so: the tiering consumer feeds it to
    /// `on_observed_invocations`' `observed_count`, which treats a
    /// too-small value as "no information" and falls back to its own count. A
    /// disarmed build therefore behaves exactly as it did before this existed,
    /// without any caller having to test the flag first.
    ///
    /// See [`crate::entry_counter`] for what the number does and does not
    /// include — inlined callees, OSR entries and the interpreted invocations
    /// that preceded this artifact's publication are all outside it.
    pub fn entry_count(&self) -> u64 {
        self._jit_entry_counter.as_ref().map_or(0, |c| c.get())
    }

    /// JIT round 19 wave 3 (lane eager3, EAGER2-4): the [`ir::TrapCause`] the
    /// optimizing builder recorded for the deopt point at `point_addr` (the
    /// stash's `point_addr`, compared for equality only), `None` for any other
    /// point, a single-pass body, or with `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`
    /// off when this body was lowered.
    pub fn trap_cause_of_point(&self, point_addr: usize) -> Option<ir::TrapCause> {
        if point_addr == 0 {
            return None;
        }
        self.trap_cause_points
            .iter()
            // Cast: a point's address as the stash records it.
            .find(|&&(p, _)| p as usize == point_addr)
            .and_then(|&(_, code)| ir::TrapCause::from_code(code))
    }

    /// [`Self::entry_count`] plus the weighted back edges (round 11 wave 18:
    /// eight single-pass back edges weigh one entry), but `None` unless it
    /// moved since the previous call (and always `None` for an uncounted body).
    /// The drain's row filter: see
    /// [`entry_counter::JitEntryCounter::take_if_moved`]. The cold-body
    /// sweeper keeps reading [`Self::entry_count`] (entries only); this does
    /// not consume it.
    pub(crate) fn entry_count_if_moved(&self) -> Option<u64> {
        self._jit_entry_counter
            .as_ref()
            .and_then(|c| c.take_if_moved())
    }

    /// The RAW (unweighted) back-edge word of this artifact's entry counter,
    /// `0` for an uncounted body (HANDLER8-2, JIT round 19 wave 9: the drain
    /// carries it to the tier census's `drained-be=` field). A relaxed hint,
    /// like [`Self::entry_count`].
    pub fn entry_back_edges(&self) -> u64 {
        self._jit_entry_counter
            .as_ref()
            .map_or(0, |c| c.back_edges())
    }

    /// JIT r20 wave 11 (lane pAlloc; books `jit-r21-nio-proposals-w16-L2.md`
    /// item 3 and `jit-r21-nio-proposals-w17-L1.md` item 8;
    /// `CRATONVM_JIT_ENTRY_VALIDATED_ONCE`, default ON, `=0` restores the
    /// per-call validation): validate this artifact's entry ONCE, when it is
    /// built, instead of on every [`Self::try_call`].
    ///
    /// Sound because the answer cannot change while a `&CompiledMethod`
    /// exists. `entry` is written only by [`Self::from_finalized_buffer`] and
    /// is always `_buffer`'s base ([`Self::entry_ptr`] asserts it); the
    /// buffer registers its range in `jit_code_regions` when it is created
    /// (`ExecutableBuffer::new` and the arena constructor) and deregisters it
    /// only in `ExecutableBuffer::drop`, which runs when this struct is dropped
    /// -- impossible while `&self` is borrowed. Code reclamation and
    /// retirement free a body only by dropping its `CompiledMethod` (the
    /// retirement queue's deferred owner), so a retired body still being
    /// called through a held reference is still mapped and still registered.
    /// Non-null and alignment are properties of the same fixed pointer. The
    /// one thing the per-call path did besides answering is the AArch64
    /// reader-side `ISB` (`synchronize_instruction_stream_for_epoch`), so the
    /// skip is x86-64 only, whose instruction fetch is coherent.
    pub(crate) fn validate_entry_once(entry: *const u8) -> *const u8 {
        if cfg!(target_arch = "x86_64")
            && validate_code_ptr(entry).is_ok()
        {
            entry
        } else {
            std::ptr::null()
        }
    }

    /// The per-call half of [`Self::validate_entry_once`]: one pointer compare
    /// in a release build when `entry` is the pointer validated at
    /// construction, the full [`validate_code_ptr`] otherwise. `entry` is the
    /// caller's ONE read of `self.entry` (the TOCTOU note in
    /// [`Self::try_call`]): the value compared is the value called.
    #[inline(always)]
    pub(crate) fn entry_call_check(&self, entry: *const u8) -> Result<(), CompileError> {
        if !entry.is_null() && entry == self.entry_validated_once {
            debug_assert!(
                validate_code_ptr(entry).is_ok(),
                "an entry validated at construction left the code regions while its artifact lives"
            );
            return Ok(());
        }
        validate_code_ptr(entry).map_err(CompileError::InvalidCodePtr)
    }

    /// Call a pure compiled method, returning `Err` instead of panicking
    /// on invalid code pointer or unsupported arg count.
    ///
    /// Round 12 wave 2: past eight words (x86-64 only) the call goes through
    /// [`call_entry_padded16`], up to [`TRY_CALL_MAX_WORDS`].
    ///
    /// task #44: this is the canonical entry point for invoking JIT
    /// code. The earlier panicking-warn-and-return-0 `call` wrapper has
    /// been removed; every VM and test caller now goes through
    /// `try_call` directly.
    ///
    /// # Safety
    /// The compiled code must match the expected signature.
    #[inline]
    pub unsafe fn try_call(&self, args: &[i64]) -> Result<i64, CompileError> {
        // Read the entry ONCE and both validate and call THAT value.
        //
        // `validate_code_ptr(self.entry)` followed by `transmute(self.entry)`
        // is a time-of-check/time-of-use hole: the two are separate loads, the
        // optimiser is free to reload, and the pointer is written through raw
        // pointers by paths this `&self` does not synchronise with. Whatever
        // produces a null there, the one guarantee worth having is that the
        // pointer we jump to is the pointer we checked.
        let entry = self.entry;
        self.entry_call_check(entry)?;
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                Ok(f())
            }
            1 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                Ok(f(args[0]))
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(args[0], args[1]))
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(args[0], args[1], args[2]))
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(args[0], args[1], args[2], args[3]))
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(args[0], args[1], args[2], args[3], args[4]))
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(args[0], args[1], args[2], args[3], args[4], args[5]))
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(
                    args[0], args[1], args[2], args[3], args[4], args[5], args[6],
                ))
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(
                    args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7],
                ))
            }
            n if n <= TRY_CALL_MAX_WORDS && cfg!(target_arch = "x86_64") => {
                Ok(call_entry_padded16(entry, None, args))
            }
            n => Err(CompileError::TooManyArgs(n)),
        }
    }

    // task #44: the panicking-warn-and-return-0 `CompiledMethod::call`
    // wrapper has been removed. All workspace callers (vm/, jit/src,
    // jit/tests) now use `try_call` directly. A JIT runtime invocation
    // failure (invalid code pointer, too-many-args) surfaces as
    // `Err(CompileError)` instead of being silently downgraded to a
    // zero return value.

    /// Call a compiled method that needs VM context, returning `Err`
    /// instead of panicking on invalid code pointer or unsupported arg
    /// count.
    ///
    /// task #44: this is the canonical entry point for invoking
    /// context-needing JIT code. The earlier
    /// panicking-warn-and-return-0 `call_with_context` wrapper has
    /// been removed; every VM and test caller now goes through
    /// `try_call_with_context` directly.
    ///
    /// # A body that does NOT want a context is routed to [`Self::try_call`]
    ///
    /// The context is not an extra argument the callee ignores — it OCCUPIES
    /// `ABI[0]`, and a body compiled without one reads its first Java argument
    /// from that same register. Supplying it anyway shifts every argument by
    /// one, so `int f(int n)` returns the CONTEXT POINTER where `n` belongs.
    ///
    /// The VM has always branched on [`Self::needs_context`] before choosing
    /// between the two entry points, so this changes nothing for it. What it
    /// fixes is a caller that cannot easily branch — a test harness that calls
    /// one way for a whole corpus — and the reason that matters is that
    /// `needs_context` is an OUTPUT OF OPTIMIZATION, not a property of the
    /// source: it is false exactly when nothing left in the graph needs the
    /// heap. So an optimization doing its job (escape analysis eliding the last
    /// `Op::New`) can flip it, and a caller that assumed it was fixed starts
    /// reading garbage — reported as a miscompile in the optimization, which is
    /// where two sessions went looking. See
    /// `docs/feature-designs/real-frame-deopt.md`.
    ///
    /// # Safety
    /// `vm_ptr` must be a valid pointer to a `SharedVm`. Args must match
    /// the method signature.
    #[inline]
    pub unsafe fn try_call_with_context(
        &self,
        vm_ptr: i64,
        args: &[i64],
    ) -> Result<i64, CompileError> {
        // See "A body that does NOT want a context" above. Routed rather than
        // refused: every caller of this function wants "invoke this method",
        // and there is no caller for whom forcing the context ABI onto a body
        // that did not ask for one is the right thing.
        if !self.needs_context() {
            return self.try_call(args);
        }
        // Read the entry ONCE — see [`CompiledMethod::try_call`] for why
        // validating one load and calling another is not the same check.
        let entry = self.entry;
        self.entry_call_check(entry)?;
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                Ok(f(vm_ptr))
            }
            1 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(vm_ptr, args[0]))
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(vm_ptr, args[0], args[1]))
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                Ok(f(vm_ptr, args[0], args[1], args[2]))
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(vm_ptr, args[0], args[1], args[2], args[3]))
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(vm_ptr, args[0], args[1], args[2], args[3], args[4]))
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(
                    vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5],
                ))
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                Ok(f(
                    vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5], args[6],
                ))
            }
            n if n < TRY_CALL_MAX_WORDS && cfg!(target_arch = "x86_64") => {
                Ok(call_entry_padded16(entry, Some(vm_ptr), args))
            }
            n => Err(CompileError::TooManyArgs(n)),
        }
    }

    // task #44: the panicking-warn-and-return-0 `call_with_context`
    // wrapper has been removed alongside `call`. All workspace callers
    // now use `try_call_with_context` directly (or the thin `.expect()`
    // alias `call_with_heap` below for test-side callers that keep an
    // `i64` return type).

    /// Backward-compatible test-only alias for `try_call_with_context`.
    ///
    /// task #44 removed the panicking `call_with_context` wrapper; this
    /// alias now delegates to the `try_*` variant and `.expect()`s the
    /// result so existing tests keep their `i64` return type. New code
    /// should call [`try_call_with_context`](Self::try_call_with_context)
    /// directly.
    ///
    /// # Safety
    /// Same safety requirements as
    /// [`try_call_with_context`](Self::try_call_with_context).
    #[inline]
    pub unsafe fn call_with_heap(&self, heap_ptr: i64, args: &[i64]) -> i64 {
        self.try_call_with_context(heap_ptr, args)
            .expect("call_with_heap: invalid JIT entry or arg count (use try_call_with_context for the fallible variant)")
    }

    /// JIT round 19 wave 4 (lane splice4, SPLICE3-1): the class this
    /// optimizing artifact's guarded receiver splice at its own `bci` TESTED
    /// (the hit edge's class), `None` for a bci it did not guarded-splice and
    /// for every other artifact. Where [`Self::dominant_receiver_at_bci`]
    /// answers `None` because the inline cache there sits on the miss edge
    /// (SPLICE2-3), this is the evidence the cache was hiding: the class the
    /// compile chose for the site. Speculation like the cache's: the consumer
    /// guards on it.
    pub fn guarded_splice_class_at_bci(&self, bci: usize) -> Option<u32> {
        let bci = u32::try_from(bci).ok()?;
        let rows = &self.ir_guarded_splice_classes;
        let at = rows.partition_point(|&(b, _)| b < bci);
        rows.get(at)
            .filter(|&&(b, class_id)| b == bci && class_id != 0)
            .map(|&(_, class_id)| class_id)
    }

    /// The receiver class this artifact's inline cache installed at `bci`, when
    /// that installation is evidence the site is monomorphic.
    ///
    /// WHY THIS EXISTS. Devirtualising a call inside a spliced body needs to
    /// know which body the receiver dispatches to, and the obvious source —
    /// `MethodProfile::receivers`, keyed by the executing method's own bci — is
    /// EMPTY at exactly the sites that matter. Measured 2026-08-18: the
    /// eager-callee-chain compiles a method like
    /// `AssertionUtils.objectsAreEqual` before it ever executes its
    /// `invokevirtual equals` interpreted, so nothing is recorded there. The
    /// evidence exists one layer down — that compiled method's own MIC
    /// installed the receiver on its first call and has been serving it since.
    ///
    /// WHAT COUNTS AS EVIDENCE, and why it is STRUCTURAL rather than statistical.
    /// The first cut of this asked for `hits + misses >= 64` with an 80% hit
    /// rate, mirroring the profile's dominance bar. That bar can essentially
    /// never be met, and it selects backwards. `record_hit` is called from the
    /// dispatch HELPER (`vm/src/jit/helpers.rs`); the JIT-emitted inline MIC
    /// compares the class id and calls the cached entry in machine code without
    /// ever entering it. So a hot, well-behaved monomorphic site reads
    /// `h0:m1` — one helper entry to install, then silence — which is exactly
    /// what the trace showed for `objectsAreEqual`, and a hit-rate bar would
    /// admit only sites that are thrashing THROUGH the helper.
    ///
    /// The structural property is stronger than any counter anyway: a populated
    /// MIC slot is monomorphic FOR ITS LIFETIME by construction. `update()`
    /// reserves an empty slot with a CAS and never retargets a populated one —
    /// "a different receiver simply takes the ordinary helper path" — because
    /// retargeting could pair one receiver's class guard with another's entry.
    /// So the class id in a populated slot is the one and only class that slot
    /// ever installed.
    ///
    /// Three things are therefore required, and each rules out a real shape:
    ///
    ///  * a populated, not-mid-installation `cached_class_id` — an empty slot
    ///    has seen nothing, and `INSTALLING_CLASS_ID` is what a concurrent
    ///    reader observes mid-publication;
    ///  * a non-zero `cached_entry_word` — `prepopulate` seeds a class id from
    ///    profile data with the entry still 0, which is a guard HINT and not an
    ///    observation. Accepting it would launder a profile guess back in as if
    ///    it were runtime evidence;
    ///  * `misses` at or below [`MIC_TO_PIC_THRESHOLD`] — every helper entry
    ///    after the install is a receiver this slot could not serve, so a site
    ///    past it has proven polymorphic. This is the ONE thing the counters do
    ///    measure honestly.
    ///
    /// WHAT IT PROMISES. Nothing about the future — it is speculation, and the
    /// consumer must guard on the class id it returns. A wrong answer costs the
    /// guard's miss edge, never correctness.
    pub fn dominant_receiver_at_bci(&self, bci: usize) -> Option<u32> {
        use std::sync::atomic::Ordering;
        // JIT round 19 wave 3 (lane splice3, SPLICE2-3): a bci this artifact
        // guarded-spliced keeps its call (and its inline cache) on the class
        // test's MISS edge only, so the cache holds a class the test rejected
        // -- the MINORITY receiver. Unconditional: the nested guarded splice
        // that reads this (`jit_bridge` `nest-profiled`) is default ON, and a
        // guard planned on the minority class only adds a compare and a body
        // to a call that still runs on almost every arrival.
        if u32::try_from(bci).is_ok_and(|b| self.ir_guarded_splice_bcis.binary_search(&b).is_ok()) {
            return None;
        }
        // A PIC at this bci that has taken a miss means the site has already
        // seen receivers its MIC could not serve. The MIC may still hold
        // whichever receiver it installed first, and guarding on that would be
        // guarding on the least informative of several.
        //
        // `misses` alone is not that evidence: only the diagnostic
        // `JitPICSlot::lookup` moves it, and nothing in production calls it
        // (r11-tier-pic-miss-counter-never-moves). What a PIC does record is
        // the classes it PUBLISHED, so read those: two distinct live classes
        // in its ways or its hashed table mean the site is polymorphic.
        let mut pic_class: Option<u32> = None;
        for p in self._jit_pic_slots.iter().filter(|p| p.bci == bci) {
            if p.misses.load(Ordering::Relaxed) > 0 {
                return None;
            }
            let published = p
                .class_ids
                .iter()
                .chain(p.mega_class_ids.iter())
                .map(|c| c.load(Ordering::Acquire))
                .filter(|&c| JitPICSlot::is_live_class_id(c));
            for c in published {
                match pic_class {
                    None => pic_class = Some(c),
                    Some(seen) if seen != c => return None,
                    Some(_) => {}
                }
            }
        }
        for slot in &self._jit_mic_slots {
            if slot.bci != bci {
                continue;
            }
            let class_id = slot.cached_class_id.load(Ordering::Relaxed);
            if JitMICSlot::is_empty_class_id(class_id)
                || class_id == JitMICSlot::INSTALLING_CLASS_ID
            {
                continue;
            }
            if slot.cached_entry_word.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if slot.misses.load(Ordering::Relaxed) > MIC_TO_PIC_THRESHOLD {
                continue;
            }
            // The PIC at this site published a class the MIC does not hold:
            // the site has seen at least two receivers.
            if pic_class.is_some_and(|c| c != class_id) {
                return None;
            }
            return Some(class_id);
        }
        None
    }

    /// `(bci, cached_class_id, hits, misses)` for every MIC slot, for the
    /// `CRATONVM_DBG_JITC` trace. The dispatch helper's counters saturate
    /// (`JitMICSlot::note_dispatch_hit` / `note_dispatch_miss`): `hits` at
    /// 4096, `misses` one past [`MIC_TO_PIC_THRESHOLD`].
    ///
    /// `dominant_receiver_at_bci` returning `None` has three causes needing
    /// three different fixes — no slot at this bci (a compile-ORDER problem), a
    /// slot with too few samples (a WARM-UP problem), and a slot that thrashes
    /// (not fixable, and correctly refused). A bare `None` cannot tell them
    /// apart, and guessing which one it was is how a session spends a build
    /// cycle on the wrong hypothesis.
    pub fn mic_slot_census(&self) -> Vec<(usize, u32, u64, u64)> {
        use std::sync::atomic::Ordering;
        self._jit_mic_slots
            .iter()
            .map(|s| {
                (
                    s.bci,
                    s.cached_class_id.load(Ordering::Relaxed),
                    s.hits.load(Ordering::Relaxed),
                    s.misses.load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// True when this artifact recorded a safe OSR entry point for `entry_pc`
    /// (i.e. [`osr_enter`](Self::osr_enter) at that pc would not bail).
    /// Lets the interpreter's OSR trigger reuse a cached compile instead of
    /// re-running the whole x64 pipeline on every trigger.
    /// Address of the OPTIMIZING tier's OSR entry stub for `bci`, if this
    /// artifact has one.
    ///
    /// The stub's ABI is `extern "C" fn(ctx: i64, locals: *const i64) -> i64`:
    /// it builds this tier's frame itself, seeds the locals the snapshot at
    /// `bci` names, and jumps into the body. That is the whole interface —
    /// contrast `osr_enter`, which passes twenty layout fields to
    /// `osr_trampoline` because the trampoline builds the single-pass frame
    /// from outside.
    ///
    /// `None` when the body published no entry at `bci`; see
    /// [`Self::ir_osr_entries`].
    pub fn ir_osr_entry_addr(&self, bci: u32) -> Option<(usize, u32)> {
        self.ir_osr_entries
            .iter()
            .find(|(b, _, _)| *b == bci)
            // Cast: an offset inside this artifact's own buffer.
            .map(|(_, off, needed)| (self.entry as usize + *off as usize, *needed))
    }

    /// The first JVM local whose offered value tag the optimizing entry stub
    /// at `bci` would read as a different kind, or `None` when every seeded
    /// local agrees (or the bci carries no kind row).
    ///
    /// `tag_of(i)` is the interpreter's `VTAG_*` for local `i`, `None` past
    /// the frame. The interpreter's OSR door asks this before
    /// [`Self::ir_osr_enter`] and, on `Some`, does not enter this body (a
    /// state-dependent answer: nothing is memoised). A reference seeded from a
    /// primitive slot would be scanned as a pointer by the body's oop maps,
    /// and a primitive seeded from a reference would be computed with.
    pub fn ir_osr_entry_kind_mismatch(
        &self,
        bci: u32,
        tag_of: impl Fn(usize) -> Option<u8>,
    ) -> Option<usize> {
        let (_, kinds) = self.ir_osr_entry_kinds.iter().find(|(b, _)| *b == bci)?;
        kinds
            .iter()
            .enumerate()
            .find(|&(i, &kind)| {
                kind != 0 && !tag_of(i).is_some_and(|t| ir_osr_slot_kind_admits(kind, t))
            })
            .map(|(i, _)| i)
    }

    /// Enter this artifact's optimizing-tier body at `bci`, seeding `locals`.
    ///
    /// The whole interface. Contrast [`Self::osr_enter`], which hands
    /// `osr_trampoline` some twenty layout fields because that trampoline
    /// builds the single-pass frame from OUTSIDE; this stub builds its own,
    /// seeds the locals the snapshot at `bci` names, and jumps into the body.
    ///
    /// `None` when there is no entry for `bci`, or when the caller offered
    /// FEWER locals than the stub reads. The second is the admission this call
    /// owes: the stub indexes `locals[i]` by JVM local index with no bound of
    /// its own, so a short slice would read past the end of the interpreter's
    /// snapshot. Refusing is free — the interpreter simply keeps running the
    /// frame it is already in.
    ///
    /// # Safety
    /// `ctx` must be this VM's context pointer, and the calling thread must be
    /// the one that installed the JIT thread pointer — the same contract every
    /// other entry into compiled code carries.
    #[cfg(target_arch = "x86_64")]
    pub unsafe fn ir_osr_enter(&self, bci: u32, ctx: i64, locals: &[i64]) -> Option<i64> {
        let (addr, needed) = self.ir_osr_entry_addr(bci)?;
        if locals.len() < needed as usize {
            return None;
        }
        // SAFETY: `addr` is inside this artifact's own executable buffer, at an
        // offset `emit_osr_entry_stubs` recorded for a stub it emitted with
        // exactly this signature.
        let f: extern "C" fn(i64, *const i64) -> i64 = std::mem::transmute(addr);
        Some(f(ctx, locals.as_ptr()))
    }

    pub fn can_osr_enter(&self, entry_pc: usize) -> bool {
        self.can_osr_enter_with(entry_pc, osr_dead_local_entry_allowed())
    }

    /// [`can_osr_enter`](Self::can_osr_enter) with the dead-local admission
    /// supplied explicitly.
    ///
    /// [`osr_dead_local_entry_allowed`] latches its answer in a `OnceLock`, so
    /// a test in this process cannot exercise both sides of the
    /// `CRATONVM_JIT_OSR_DEAD_LOCALS` kill switch through the public entry
    /// point — and that switch is the documented escape hatch for this
    /// relaxation, so it is exactly the path that wants coverage.
    pub fn can_osr_enter_with(&self, entry_pc: usize, allow_dead_locals: bool) -> bool {
        // Diagnosis lever, default OFF. `Arrays.sort(long[])` on >=1000
        // elements throws an AIOOBE with a garbage index under OSR
        // (`probes/SortProbe.java`), and the trace shows artifacts compiled
        // for one pc being entered at another (`partitionDualPivot` compiled
        // at 117, entered at 93; `mixedInsertionSort` compiled at 282, entered
        // at 368). Restricting entry to the compiled pc separates "the second
        // entry point is wrong" from "the body is wrong".
        if osr_single_pc_entry_only() {
            if let Some(compiled_pc) = self.osr_compiled_entry_pc {
                if compiled_pc != entry_pc {
                    return false;
                }
            }
        }
        if !allow_dead_locals
            && self
                .osr_dead_mask
                .as_ref()
                .and_then(|m| m.get(entry_pc).copied())
                .unwrap_or(0)
                != 0
        {
            return false;
        }
        self.osr_pc_to_native
            .as_ref()
            .and_then(|t| t.get(entry_pc).copied())
            .map_or(false, |off| off >= 0)
    }

    /// OSR entry: enter JIT code at an arbitrary bytecode PC with interpreter locals.
    ///
    /// # Safety
    /// `vm_ptr` must be a valid SharedVm pointer. `jit_locals` must contain exactly
    /// `osr_num_locals` i64 values in local-index order. `thread_ptr` must be the
    /// current `*mut JvmThread` (whose shadow stack is already allocated by
    /// `set_jit_thread`) when shadow-stack frame slots exist, so this
    /// OSR-entered frame is precisely tracked; pass 0 to opt out of shadow
    /// tracking in tests.
    #[cfg(target_arch = "x86_64")]
    #[inline(never)]
    pub unsafe fn osr_enter(
        &self,
        vm_ptr: i64,
        jit_locals: &[i64],
        entry_pc: usize,
        thread_ptr: i64,
    ) -> Option<i64> {
        let pc_to_native = self.osr_pc_to_native.as_ref()?;
        if entry_pc >= pc_to_native.len() {
            return None;
        }
        let native_offset = pc_to_native[entry_pc];
        if native_offset < 0 {
            return None;
        }
        let target_addr = self.entry as usize + native_offset as usize;

        // A nonzero dead mask names the locals the trampoline must NOT seed:
        // each is dead here and shares its home register with a live local, so
        // loading it would clobber the live owner. The trampoline skips exactly
        // those (`dead_mask >> i & 1` → `continue`); see
        // [`osr_dead_local_entry_allowed`] for why that is sufficient, and for
        // the `CRATONVM_JIT_OSR_DEAD_LOCALS=0` kill switch that restores the
        // historical "refuse the entry outright" behaviour.
        let dead_mask = self
            .osr_dead_mask
            .as_ref()
            .and_then(|m| m.get(entry_pc).copied())
            .unwrap_or(0);
        if dead_mask != 0 && !osr_dead_local_entry_allowed() {
            return None;
        }

        osr_trampoline(
            target_addr,
            vm_ptr,
            jit_locals.as_ptr(),
            self.osr_num_locals,
            self.osr_num_reg_locals,
            self.osr_local_assignments.as_deref(),
            self.osr_xmm_assignments.as_deref(),
            self.osr_frame_size,
            self.osr_callee_saved_base,
            self.osr_callee_saved_regs.as_deref(),
            self.osr_callee_saved_xmms.as_deref(),
            self.osr_xmm_saved_base,
            self.osr_heap_local_offset,
            self.jit_thread_slot_off,
            self.stack_floor_slot_off,
            self.osr_frame_record,
            self.needs_context,
            dead_mask,
            self.shadow_thread_slot_off,
            self.shadow_savetop_slot_off,
            self.shadow_off_in_thread,
            self.compile_id,
            self.sp_id_slot_off,
            thread_ptr,
        )
    }
}

// ---------------------------------------------------------------------------
// JIT invoke info and MIC types
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Method inlining
// ---------------------------------------------------------------------------

/// Maximum callee bytecode size (excluding padding) eligible for inlining at
/// **any** call site — the admission gate, not the policy.
///
/// # 2026-07-26 re-shape (jit-inlining-and-ir-calls)
///
/// This was `35`, applied as a flat cap to every callee. That number is
/// HotSpot's `MaxInlineSize`, which HotSpot applies **only to cold callees**;
/// for a callee reached from a hot call site HotSpot uses `FreqInlineSize`,
/// which is `325` — nearly 10x larger. CratonVM had the cold constant and no
/// hot tier at all, so a 40-byte accessor called a million times in a loop was
/// treated exactly like a 40-byte method called twice.
///
/// The three-tier HotSpot shape now lives here:
///
/// | tier | callee cap | expansion cap | HotSpot name | when |
/// |---|---|---|---|---|
/// | trivial | [`MAX_TRIVIAL_INLINE_SIZE`] = 6 | — | `MaxTrivialSize` | always: smaller than the call sequence it replaces |
/// | cold | [`MAX_INLINE_SIZE_COLD`] = 35 | [`MAX_INLINE_EXPANSION_COST`] = 64 | `MaxInlineSize` | no profile evidence the site is hot |
/// | hot | `MAX_INLINE_BYTECODE_SIZE` = 325 | [`MAX_INLINE_EXPANSION_COST_HOT`] = 512 | `FreqInlineSize` | site in a hot loop, or a hot receiver profile |
///
/// **This constant is the value the VM-side inline resolver
/// (`vm/src/runtime/interpreter.rs::resolve_inline_site`) gates on**, and it
/// must stay the largest of the three: that resolver is a pure admission
/// filter handing candidates to the planner in `try_compile_inner`, which then
/// applies the per-site tier via [`inline_site_expansion_cost_tiered`].
/// Lowering this constant back to 35 would make the hot tier unreachable.
///
/// See `jit-inlining-and-ir-calls.md`.
pub const MAX_INLINE_BYTECODE_SIZE: usize = 325;

/// Callee-size cap for a call site with **no evidence of hotness** —
/// HotSpot's `MaxInlineSize`. This is exactly the value the flat
/// pre-2026-07-26 cap used, so a cold site's inlining decisions are unchanged
/// by the re-shape.
pub const MAX_INLINE_SIZE_COLD: usize = 35;

/// Callee size below which inlining is **always** profitable regardless of site
/// hotness — HotSpot's `MaxTrivialSize`. A callee this small is smaller than
/// the call sequence that would invoke it.
pub const MAX_TRIVIAL_INLINE_SIZE: usize = 6;

/// Total inlined expansion budget per compiled method, for a caller with no
/// hot-loop evidence.
///
/// Raised from 250. The budget is not a free parameter: the single-pass backend
/// sizes both its code buffer and its frame from the planned sites
/// (`x64::compile` reserves `callee_code_len * 64` buffer bytes and
/// `callee_max_locals + callee_code_len` spill slots **per inlined site**), so
/// this ceiling directly bounds committed executable memory and JIT frame size.
/// 750 keeps both comfortably bounded — order 48 KiB of buffer estimate and
/// 6 KiB of frame in the worst case — while being 3x the old ceiling.
pub const MAX_INLINE_BUDGET: usize = 750;

/// Total inlined expansion budget for a **hot** caller — one the profile shows
/// executing a hot loop, which is where inlining actually pays. Worst case
/// order 128 KiB of buffer estimate and 16 KiB of frame; see
/// [`MAX_INLINE_BUDGET`] for where those numbers come from.
pub const MAX_INLINE_BUDGET_HOT: usize = 2000;

/// Maximum estimated native-code expansion accepted for one **cold** inline
/// site.
///
/// Bytecode length alone under-prices field accesses and helper-dependent
/// bodies. Keeping a second, backend-oriented ceiling prevents one nominally
/// small leaf from consuming disproportionate instruction-cache space.
pub const MAX_INLINE_EXPANSION_COST: usize = 64;

/// Maximum estimated native-code expansion accepted for one **hot** inline
/// site. The hot counterpart of [`MAX_INLINE_EXPANSION_COST`], scaled with the
/// callee-size tier (35 → 325 is ~9x; 64 → 512 is 8x) so the two ceilings stay
/// proportionate and neither becomes the sole binding constraint.
pub const MAX_INLINE_EXPANSION_COST_HOT: usize = 512;

/// Back-edge execution count at which a loop counts as hot for inlining.
/// Matches the threshold `profile::LoopTripProfile::suggests_unroll_factor`
/// already uses for "not hot enough", so the two profile consumers agree on
/// what "hot" means.
pub const INLINE_HOT_LOOP_BACKEDGES: u64 = 100;

/// Observation count at which a *call site* (rather than a loop) counts as hot,
/// from its receiver-type profile. `MethodProfile::receivers` records one
/// observation per executed `invokevirtual`/`invokeinterface`, so the summed
/// count is a direct execution count for that site.
pub const INLINE_HOT_SITE_OBSERVATIONS: u32 = 500;

/// The kind byte [`CompiledMethod::ir_osr_entry_kinds`] records for a local
/// the optimizing OSR entry stub seeds as a value of type `ty`: `b'I'`, `b'J'`,
/// `b'F'`, `b'D'`, `b'L'`, and `0` (no constraint) for a type that is not a
/// JVM value.
pub fn ir_osr_slot_kind(ty: ir::IrType) -> u8 {
    match ty {
        ir::IrType::Int => b'I',
        ir::IrType::Long => b'J',
        ir::IrType::Float => b'F',
        ir::IrType::Double => b'D',
        ir::IrType::Ref => b'L',
        ir::IrType::Void | ir::IrType::Control | ir::IrType::Memory => 0,
    }
}

/// Does an interpreter slot tagged `vtag` (`cratonvm_types::VTAG_*`) hold a
/// value of `kind` ([`ir_osr_slot_kind`])? Exact per JVM type, with `null`
/// accepted as a reference. An uninitialised slot, a `returnAddress` and an
/// unknown tag satisfy no kind; `0` accepts anything.
pub fn ir_osr_slot_kind_admits(kind: u8, vtag: u8) -> bool {
    match kind {
        0 => true,
        b'I' => vtag == cratonvm_types::VTAG_INT,
        b'J' => vtag == cratonvm_types::VTAG_LONG,
        b'F' => vtag == cratonvm_types::VTAG_FLOAT,
        b'D' => vtag == cratonvm_types::VTAG_DOUBLE,
        b'L' => vtag == cratonvm_types::VTAG_OBJECT || vtag == cratonvm_types::VTAG_NULL,
        _ => false,
    }
}

/// Is every `Integer.valueOf(int)`, `Function.apply(Object)`,
/// `checkcast Double`, `Double.doubleValue()` run in this method proven by its
/// constant pool, with at least one such run? Decides whether either tier may
/// fold those runs into one `jit_lambda_int_to_double` call (the single-pass
/// `BackendRequest::lambda_int_to_double_proven`, the optimizing
/// `IrBuilder::tdigest_scalar_kernel`).
///
/// Interpreter round i1 wave 10 (lane L5,
/// `interpreter-L6-jit-crate-tdigest-dist-name-peepholes-FIXED`): the fold used to be armed
/// by the compiling method's NAME (`org/elasticsearch/tdigest/Dist.quantile` /
/// `.cdf`), an application class-name allow-list, and each tier's run check
/// (`tdigest_adapter_members_match` / `tdigest_adapter_run_matches`) accepts a
/// site whose dispatch row is absent. The name was therefore also the proof
/// that an unrowed `invokestatic` there was `Integer.valueOf`. This asks the
/// constant pool instead: each run's four references must be exactly those
/// members (`java/*` names, which only the boot loader defines), so a
/// method-wide `true` makes every run a tier can fold one of these.
///
/// A run is an `invokestatic`, `invokeinterface`, `checkcast`, `invokevirtual`
/// at `pc`, `pc + 3`, `pc + 8`, `pc + 11` on instruction boundaries. `false`
/// with no resolver answer, and for any adapter-shaped run that does not
/// resolve to the four members.
pub fn lambda_int_to_double_runs_proven(
    code: &[u8],
    code_len: usize,
    invoke: &dyn Fn(u16) -> Option<(String, String, String)>,
    class_name: &dyn Fn(u16) -> Option<String>,
) -> bool {
    const VALUE_OF: (&str, &str, &str) = ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
    const APPLY: (&str, &str, &str) = (
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    );
    const DOUBLE_VALUE: (&str, &str, &str) = ("java/lang/Double", "doubleValue", "()D");
    let end = code_len.min(code.len());
    let cp = |at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
    };
    let member = |at: usize, want: (&str, &str, &str)| -> bool {
        cp(at)
            .and_then(invoke)
            .is_some_and(|(c, m, d)| c == want.0 && m == want.1 && d == want.2)
    };
    let mut runs = 0usize;
    let mut pc = 0usize;
    while pc < end {
        if code[pc] == 0xb8
            && pc + 14 <= end
            && code[pc + 3] == 0xb9
            && code[pc + 8] == 0xc0
            && code[pc + 11] == 0xb6
        {
            let proven = member(pc + 1, VALUE_OF)
                && member(pc + 4, APPLY)
                && cp(pc + 9)
                    .and_then(class_name)
                    .is_some_and(|name| name == "java/lang/Double")
                && member(pc + 12, DOUBLE_VALUE);
            if !proven {
                return false;
            }
            runs += 1;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    runs > 0
}

/// Bytecode pc ranges `[header, back_edge]` of every loop the profile shows to
/// be hot, derived from `MethodProfile::loops` (keyed by back-edge pc) plus the
/// bytecode itself, which is what recovers each back-edge's target — i.e. the
/// loop header.
///
/// `LoopTripProfile` records only the back-edge pc, so the header is recovered
/// by decoding the branch at that pc: a 3-byte `goto`/conditional carries a
/// signed 16-bit relative offset, a 5-byte `goto_w` a signed 32-bit one. A
/// back-edge is a branch whose target is at or before its own pc; anything else
/// keyed here is ignored rather than guessed at.
///
/// Empty whenever there is no profile, which makes every site cold and every
/// budget collapse to its pre-2026-07-26 value.
pub(crate) fn hot_loop_ranges(
    code: &[u8],
    code_len: usize,
    profile: Option<&profile::MethodProfile>,
) -> Vec<(usize, usize)> {
    let Some(prof) = profile else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    for (&back_pc, trips) in &prof.loops {
        if trips.backedge_count < INLINE_HOT_LOOP_BACKEDGES {
            continue;
        }
        // Round 14 wave 5 (lane fixup2, RV4-4): the shared decoder; anything
        // keyed here that is not a backward branch is ignored.
        if let Some(header) = loop_back_edge_header(code, code_len, back_pc) {
            ranges.push((header, back_pc));
        }
    }
    ranges
}

/// The loop header a back edge at `pc` closes: the target of the branch at
/// `pc` when it is one of `if<cond>`, `if_icmp<cond>`, `if_acmp<cond>`,
/// `goto`, `ifnull`/`ifnonnull` (3 bytes, 16-bit offset) or `goto_w` (5 bytes,
/// 32-bit offset) and the target is at or before `pc`. `jsr`/`jsr_w` are
/// subroutine calls, never loop back edges, and a switch back edge is not a
/// loop here. `None` when the operands run past `code_len`.
///
/// Round 14 wave 5 (lane fixup2, proposal RV4-4 of
/// `jit-r14-review4-proposals-RETIRED-20260930.md`): the ONE back-edge decoder of
/// [`hot_loop_ranges`], [`bytecode_loop_ranges`], [`osr_entry_loop_ranges`]
/// and [`bytecode_pc_in_loop`], which each carried a private copy of this
/// match (and were one edit away from disagreeing on `goto_w`). The offset
/// itself is `bytecode_analysis::offset_branch_target`'s.
pub(crate) fn loop_back_edge_header(code: &[u8], code_len: usize, pc: usize) -> Option<usize> {
    let len = code_len.min(code.len());
    if !matches!(*code[..len].get(pc)?, 0x99..=0xa7 | 0xc6..=0xc8) {
        return None;
    }
    let target = bytecode_analysis::offset_branch_target(&code[..len], pc)?;
    (target <= pc).then_some(target)
}

/// Whether a call site at `pc` is HOT, and therefore eligible for the
/// [`MAX_INLINE_BYTECODE_SIZE`] (`FreqInlineSize`) allowance rather than the
/// [`MAX_INLINE_SIZE_COLD`] (`MaxInlineSize`) one.
///
/// Two independent pieces of real profile evidence, either of which suffices:
///
///  1. the site lies inside a loop whose back-edge count clears
///     [`INLINE_HOT_LOOP_BACKEDGES`] (`MethodProfile::loops`); or
///  2. the site's own receiver-type profile records at least
///     [`INLINE_HOT_SITE_OBSERVATIONS`] executions (`MethodProfile::receivers`,
///     populated per executed virtual/interface invoke).
///
/// With no profile every site is cold, so an unprofiled compile inlines exactly
/// as it did before the re-shape.
pub(crate) fn call_site_is_hot(
    pc: usize,
    hot_loops: &[(usize, usize)],
    profile: Option<&profile::MethodProfile>,
) -> bool {
    if hot_loops
        .iter()
        .any(|&(header, back)| pc >= header && pc <= back)
    {
        return true;
    }
    profile
        .and_then(|p| p.receivers.get(&pc))
        .map(|counts| {
            counts.values().copied().map(u64::from).sum::<u64>()
                >= u64::from(INLINE_HOT_SITE_OBSERVATIONS)
        })
        .unwrap_or(false)
}

/// Round 14 wave 4 (lane calls2, proposal C14W3-1 of
/// `jit-r14-calls3-proposals-RETIRED-20260930.md`): `CRATONVM_JIT_INLINE_LOOP_SITES_HOT`
/// (**default ON**; `0` prices a method-entry compile's call sites from the
/// profile alone, as before). Read once per compile, live.
pub(crate) fn inline_loop_sites_hot_enabled() -> bool {
    true
}

/// Every loop of `code` as a `[header, back_edge]` range: the target and the
/// source of each backward branch in [`bytecode_pc_in_loop`]'s set (`if*`,
/// `goto`, `ifnull`/`ifnonnull`, `goto_w`; a switch back edge and `jsr` are not
/// loops here). Duplicates are dropped; order is bytecode order.
pub(crate) fn bytecode_loop_ranges(code: &[u8], code_len: usize) -> Vec<(usize, usize)> {
    let len = code_len.min(code.len());
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut p = 0usize;
    while p < len {
        if let Some(t) = loop_back_edge_header(code, len, p) {
            if !ranges.contains(&(t, p)) {
                ranges.push((t, p));
            }
        }
        p += bytecode_analysis::step(code, p).max(1);
    }
    ranges
}

/// The loop ranges a METHOD-ENTRY compile prices its call sites by
/// ([`call_site_is_hot`]) and picks its whole-method budget from.
///
/// [`hot_loop_ranges`] when the profile carries loop evidence. When it carries
/// none -- `MethodProfile::loops` is written only under `CRATONVM_TIER_PGO`
/// (off by default), so this is every default compile, and every compile with
/// no profile at all -- every loop of the method counts, under
/// [`inline_loop_sites_hot_enabled`]: the compile itself is the frequency
/// evidence (the argument OD-1 makes for the OSR door,
/// [`osr_entry_loop_ranges`]). A method is compiled because it ran hot, and a
/// site inside one of its loops runs at least as often as the method does.
/// Without this, a static or `invokespecial` site -- which has no receiver
/// profile either -- is priced cold at every default method-entry compile,
/// so an in-loop static helper between 36 and 325 bytes stays a call
/// (`r14w3-calls-static-sites-always-priced-cold-FIXED-20260929.md`).
///
/// A profile WITH loop evidence is left alone: a loop it shows cold is cold.
/// Sites outside every loop keep the cold pricing either way.
pub(crate) fn method_entry_hot_loop_ranges(
    code: &[u8],
    code_len: usize,
    profile: Option<&profile::MethodProfile>,
) -> Vec<(usize, usize)> {
    let mut ranges = hot_loop_ranges(code, code_len, profile);
    if profile.is_none_or(|p| p.loops.is_empty()) && inline_loop_sites_hot_enabled() {
        for range in bytecode_loop_ranges(code, code_len) {
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    }
    ranges
}

/// `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` — let an allocation-bearing method take
/// the C1->C2 supersede. OPT-IN, deliberately.
///
/// The refusal it lifts is stale (see [`c2_upgrade_would_engage`]), but lifting
/// it was measured NEUTRAL on the workload that motivated it, and it has a real
/// regression mechanism that has NOT been priced: at C2 a surviving allocation
/// lowers through the shared `jit_new_object` stub, where the single-pass body
/// emits an inline TLAB bump. A method whose allocations escape therefore trades
/// a cheaper allocation for a more optimized body, and nothing measured here
/// says which way that lands.
///
/// So this ships as a lever to price the trade on the gauntlet, not as a
/// default. Turning it on is the A arm; leaving it off is every prior build.
///
/// # Priced (round 9 wave 3): a regression, so it stays OFF
///
/// `NOTES-w3-tiering3.md` measured it interleaved, 3 reps per arm, outputs
/// matching HotSpot throughout: `CratonBenchC2 bind` runs 1 768 ms by default
/// and **3 275 ms** with `=1` (1.85x slower; 3 708 ms with the IR TLAB bump
/// on as well), CratonBenchC2 total 2 429 -> 3 945 ms, and no probe it covered
/// gained outside noise. The mechanism, from `print-compilation`:
/// `Registry.text` and `Registry.intValue` (both allocate) are superseded at C2
/// as 5.5-6 KB bodies that are slower than their C1 ones. The hotness door the
/// default-on entry counter (`crate::entry_counter`) opened instead admits only
/// methods whose OWN entries are hot -- the population where C2 pays -- which
/// is why the structural lift here is no longer needed either.
pub(crate) fn c2_alloc_upgrade_enabled() -> bool {
    // Still OPT-IN, for the MEASURED reason in the doc above (round 9 wave
    // 3: `CratonBenchC2 bind` 1.85x slower with `=1`), not for a defect.
    // An earlier note here blamed the IR inline TLAB bump
    // (`RJitMapTierDiff` 4 runs in 10); that bump is default ON since round
    // 9 wave 4 after a 30/30 soak — see `ir_inline_tlab_enabled`'s doc in
    // `ir_lower.rs`. Hot allocating methods reach C2 through the hotness
    // door instead; the optimizing OSR door's `callee_compiler` is the one
    // route that bypasses this gate (page
    // `r11w3-tier-c2-alloc-upgrade-gate-is-stale-and-bypassed`).
    // Read per tier decision, no `static` (j27 LATCHES).
    false
}

/// May a method containing `anewarray` still plan its invokes?
///
/// Default yes. `=0` restores the historical coupling, where one reference-array
/// allocation anywhere in a method discarded `invoke_info` for the whole method
/// and the builder then refused it at its first invoke.
pub(crate) fn ir_calls_with_anewarray_enabled() -> bool {
    true
}

/// C1→C2 supersede eligibility: would an `optimize=true` recompile of this
/// method actually take the optimizing IR pipeline AND be expected to produce
/// better code than the single-pass body it replaces?
///
/// Mirrors the IR gate in `try_compile_inner` (`ir_compatible` + the
/// category-2/FP admission clauses) with one deliberate extra restriction:
/// only ALLOCATION-FREE methods qualify, and a call-bearing method qualifies
/// only when the IR can lower its calls as well as the single-pass backend
/// does. Pure compute (int/long/FP arithmetic over locals, arrays and fields —
/// sieve/matrix/reduction loop shapes) is where the IR backend reliably wins.
///
/// # Why calls used to be excluded outright, and what changed
///
/// This predicate used to reject ANY method containing an invoke, because the
/// IR path lowered every invoke through the generic `invoke_dispatch` helper —
/// no direct calls, no inline caches — so an "optimizing" recompile of a
/// call-bearing method could be a net REGRESSION over the single-pass body,
/// which has always had direct calls and constructor inlining. That was the
/// binding constraint on the whole IR pipeline: the tiered manager compiles a
/// hot call-bearing method at C1 (`optimize == false`, which skips the IR
/// pipeline entirely) and then declined to promote it here, so raising
/// `ir_compatible`'s invoke cap alone changed nothing under tiered routing.
///
/// `ir_lower::emit_direct_cross_call` removed that per-call tax for the
/// STATICALLY BOUND invoke kinds, so those are now admitted:
///
///  * `invokestatic` (0xb8) and `invokespecial` (0xb7) have exactly one
///    possible target, so the IR binds them with the same raw `CALL` the
///    single-pass backend emits — subject to `ir_direct_calls_enabled()`,
///    without which the IR would be back to paying the helper per call and the
///    original rejection is still the right answer.
///  * `invokevirtual` (0xb6) / `invokeinterface` (0xb9) used to stay excluded,
///    because the IR had no inline cache and a virtual site paid the full
///    helper round trip WITH a dynamic target lookup. They are now admitted:
///    `ir_lower::emit_inline_cache_call` (jit-inlining-and-ir-calls) gives such
///    a site the same monomorphic-then-4-way-polymorphic cascade the
///    single-pass backend emits, and `CRATONVM_JIT_IR_CALL_VIRTUAL` inverted
///    from opt-in to opt-out with it. Admission is subject to
///    caller-supplied virtual-call policy for the same reason the statically-bound
///    kinds are subject to `ir_direct_calls_enabled()`: with the capability
///    switched off, the IR is back to paying the helper per call and the
///    original rejection is still the right answer.
///
/// # Allocation-bearing methods: excluded until 2026-08-27, and why that ended
///
/// This predicate used to refuse any method containing a `new`, on two stated
/// grounds. The second — "the IR call eligibility loop requires
/// `new_ops.is_empty()` anyway, so an allocation-bearing method's invokes would
/// bail the builder" — was already FALSE when it was written down: cov-04
/// increment 2 deleted that conjunct (`call_eligible` is now
/// `scan.anewarray_ops.is_empty()` alone) with the note that "the term outlived
/// its reason". A live run says the same thing out loud:
/// `invoke-plan VolumeShort2.loadFromArray: new_ops=1 ... call_eligible=true`.
///
/// The first ground — that the IR lowers an allocation through the shared
/// `jit_new_object` stub where single-pass emits an inline TLAB bump — is real,
/// but it is an argument about a SURVIVING allocation. The allocations this
/// gate kept out are exactly the ones escape analysis exists to DELETE, and EA
/// runs only at C2. So the gate was self-defeating: an allocation-bearing
/// method could never reach the tier that removes its allocations.
///
/// Measured on kfusion's per-voxel `Short2` (`VolumeShort2.loadFromArray`, one
/// `new` per voxel, 16.7M voxels a frame): the method was pinned at C1 and its
/// allocation survived at ~708 ns/voxel of the 755 ns total, against ~2 ns on
/// HotSpot, whose EA scalar-replaces the same object.
///
/// `anewarray` and `indy` stay excluded and keep their own reasons:
/// `IrBuilder::build` has no `0xbd` arm at all, and `ir_compatible` refuses
/// `indy` a stage earlier.
///
/// The refusal is therefore LIFTABLE, but lifting it is opt-in
/// (`CRATONVM_JIT_C2_ALLOC_UPGRADE=1`) rather than default: doing so measured
/// neutral on the kfusion voxel probe, because that allocation escapes its
/// method by being RETURNED (`loadFromArray` ends in `areturn`), and per-method
/// EA cannot scalar-replace an escaping object however good its tier. Killing
/// it needs the accessor chain INLINED into the consuming loop first, then EA
/// on the merged graph — which is a capability, not a gate, and which landed on
/// 2026-08-27 (`CRATONVM_JIT_IR_INLINE`). It is still not THIS gate that pays:
/// the win is measured in the CALLER, whose own bytecode contains no `new` and
/// which therefore never needed this lift at all. See
/// `c2_alloc_upgrade_enabled` for the regression mechanism that keeps this off
/// by default.
///
/// This predicate deliberately stays a cheap, scan-only approximation (it
/// cannot resolve a constant pool), so it can admit a method the IR later
/// bails on — e.g. a constructor whose `invokespecial` is an `<init>` super
/// call. That costs a wasted optimizing compile whose result is discarded in
/// favour of the single-pass body; it is never a correctness risk.
/// Returns `true` when the `new` (0xbb) at `new_pc` lies inside a straight-line
/// throw-only block (`new` -> constants/loads/stack shuffles/`getstatic`/`getfield`/
/// constructor `invokespecial` -> `athrow`) with no branches or returns.
fn new_site_is_immediate_throw_block(code: &[u8], code_len: usize, new_pc: usize) -> bool {
    let code_len = code_len.min(code.len());
    let Some(first) = crate::bytecode_analysis::decode_at(code, code_len, new_pc) else {
        return false;
    };
    if first.op != 0xbb {
        return false;
    }
    let mut pc = first.next_pc();
    while pc < code_len {
        let Some(insn) = crate::bytecode_analysis::decode_at(code, code_len, pc) else {
            return false;
        };
        match insn.op {
            0xbf => return true,
            0x00..=0x2d | 0x57..=0x5f | 0xb2 | 0xb4 | 0xb6..=0xb8 | 0xba | 0xbb => {
                pc = insn.next_pc();
            }
            _ => return false,
        }
    }
    false
}

pub fn c2_upgrade_would_engage(
    code: &[u8],
    code_len: usize,
    descriptor: &str,
    ir_emit_long: bool,
    ir_emit_fp: bool,
    ir_emit_virtual_calls: bool,
) -> bool {
    let Some(scan) = x64::jit_scan(code, code_len, descriptor) else {
        return false;
    };
    if !scan.anewarray_ops.is_empty() {
        return false;
    }
    // An `invokedynamic` no longer disqualifies the method: `IrBuilder`'s
    // `0xba` arm replaces the site with an uncommon trap and compiles the rest,
    // the same trade the single-pass backend already makes. With the kill
    // switch set, the old refusal is exactly restored -- and it has to be here
    // as well as in `ir_compatible`, or the supersede door and the admission
    // gate would disagree about the same method and the flag would only half
    // work, which is the failure `compile_gate` exists to prevent.
    if !scan.indy_ops.is_empty() && !ir::ir_site_trap_enabled() {
        return false;
    }
    if !scan.new_ops.is_empty()
        && !c2_alloc_upgrade_enabled()
        && !scan
            .new_ops
            .iter()
            .all(|&(new_pc, _)| new_site_is_immediate_throw_block(code, code_len, new_pc))
    {
        return false;
    }
    if !scan.invoke_ops.is_empty() {
        // Every invoke must lower at least as well as single-pass would. The
        // statically-bound kinds need the direct-call capability; the
        // dynamically-bound kinds need the inline-cache capability. Anything
        // else (there is nothing else — `indy` is rejected above) is refused.
        let statics_ok = ir_direct_calls_enabled();
        let virtuals_ok = ir_emit_virtual_calls;
        let all_lowerable = scan.invoke_ops.iter().all(|(_, _, opcode)| match *opcode {
            0xb8 | 0xb7 => statics_ok,
            0xb6 | 0xb9 => virtuals_ok,
            _ => false,
        });
        if !all_lowerable {
            return false;
        }
    }
    if !ir::ir_compatible_sized(&scan, code_len) {
        return false;
    }
    ir_value_shape_admitted(code, code_len, descriptor, ir_emit_long, ir_emit_fp)
}

/// The optimizing tier's VALUE-SHAPE admission term: may a method with this
/// signature and body reach the IR pipeline under these two gates?
///
/// `(!category2 && !fp) || (ir_emit_long && !fp) || (ir_emit_fp && fp_in_body)`
/// — the conjunct `try_compile_inner`'s admission chain, its printed verdict
/// and [`c2_upgrade_would_engage`] each used to spell out by hand, three times,
/// with nothing to keep the copies in step. One spelling now, so the supersede
/// door, the admission gate and the verdict line cannot disagree about a
/// method.
///
/// Also cheaper than the spelled-out form, which is a linear bytecode walk per
/// term: it walked the body for floating-point opcodes TWICE (`method_uses_fp`
/// in the first two disjuncts) and for category-2 opcodes even when the long
/// gate alone already admitted the method. Factored as
/// `!fp && (ir_emit_long || !category2)`, else `ir_emit_fp && fp_in_body` — an
/// exact rewrite, since `method_uses_fp` is `fp_in_descriptor || fp_in_body`,
/// so `fp_in_body` cannot hold where `method_uses_fp` does not.
pub(crate) fn ir_value_shape_admitted(
    code: &[u8],
    code_len: usize,
    descriptor: &str,
    ir_emit_long: bool,
    ir_emit_fp: bool,
) -> bool {
    if !method_uses_fp(code, code_len, descriptor) {
        ir_emit_long || !method_uses_category2(code, code_len, descriptor)
    } else {
        ir_emit_fp && fp_in_body(code, code_len)
    }
}

/// Structural half of the early optimized-tier admission for the common
/// `static int f(int)` self-recursion shape. The VM performs the constant-pool
/// identity check separately; this function deliberately accepts only scalar,
/// allocation-free bytecode whose calls are all `invokestatic`.
pub fn scalar_selfrec_ir_would_engage(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    if descriptor != "(I)I" {
        return false;
    }
    let Some(scan) = x64::jit_scan(code, code_len, descriptor) else {
        return false;
    };
    if scan.invoke_ops.is_empty()
        || scan.invoke_ops.iter().any(|(_, _, opcode)| *opcode != 0xb8)
        || !scan.multianewarray_ops.is_empty()
        || !scan.field_ops.is_empty()
        || !scan.typecheck_ops.is_empty()
        || !scan.static_field_ops.is_empty()
        || !scan.new_ops.is_empty()
        || !scan.anewarray_ops.is_empty()
        || !scan.indy_ops.is_empty()
        || scan.has_newarray
        || scan.has_athrow
    {
        return false;
    }
    ir::ir_compatible_sized(&scan, code_len)
        && !method_uses_category2(code, code_len, descriptor)
        && !method_uses_fp(code, code_len, descriptor)
}

/// A call the CALLEE's own bytecode makes, resolved against the CALLEE's
/// constant pool.
///
/// Why this type exists at all: every other piece of `InlineSite` metadata
/// (`field_info`, `static_field_info`, `ldc_info`) is keyed by CALLEE pc and
/// resolved against the CALLEE's constant pool, because a spliced body's
/// bytecode indices name entries in ITS class's pool, not the caller's. Invokes
/// had no such entry — the resolver rejected every `invoke*` outright, so no
/// call inside a spliced body was ever emittable and nothing needed resolving.
///
/// The top-level `invoke_info` cannot be reused for this: it is keyed by CALLER
/// pc, a different bytecode space entirely, and a callee pc that happens to
/// collide with a caller pc would silently select an unrelated target.
///
/// Owned `String`s rather than the `&'static str`s [`JitInvokeInfo`] wants,
/// because an `InlineSite` outlives no particular compile and crosses the
/// crate boundary by value. `try_compile_inner` interns each of these into the
/// compile's own `_jit_strings` / `_jit_invoke_infos` arenas immediately before
/// backend emission and records the resulting pointer in
/// [`InlineSite::resolved_invoke_infos`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InlineInvokeTarget {
    /// Constant-pool class name of the target, as the CALLEE's pool spells it.
    pub class_name: String,
    /// Target method name.
    pub method_name: String,
    /// Target descriptor.
    pub descriptor: String,
    /// Operand slots the call consumes, receiver INCLUDED — the same count
    /// `JitInvokeInfo::num_jit_args` carries and the same one the emitter pops.
    pub num_jit_args: usize,
    /// Descriptor return byte (`b'V'` for void).
    pub return_type: u8,
    /// `jit_invoke_dispatch`'s encoding: 0 = virtual, 1 = special, 2 =
    /// interface, 3 = static. Never 4 (the IR-only self-recursive marker).
    pub invoke_kind: u8,
    /// `ObjectHeader` class id of the class whose constant pool named this
    /// target — the CALLEE's declaring class, not the enclosing method's.
    ///
    /// Load-bearing for exactly the reason `JitInvokeInfo::declaring_class_id`
    /// is (BUG-JIT-INVOKESPECIAL-LOADER-20260726): a class NAME is not a class
    /// identity, and a spliced body resolved through the wrong loader's copy
    /// picks a different method with the same name. The enclosing method's id
    /// would be the wrong answer whenever the callee comes from another loader.
    pub declaring_class_id: u32,
    /// `ObjectHeader` class id named by the target's constant-pool entry, when
    /// already loaded. Distinct from `declaring_class_id` (the class whose
    /// pool contains the call) and `resolved_class_id` (the class that
    /// actually declares the selected method). Late static cells require this
    /// identity so same-named classes from another loader cannot fill them.
    pub target_class_id: u32,
    /// `ObjectHeader` class id of the class that DECLARES the method an
    /// `invokestatic` row (`invoke_kind == 3`) resolves to -- the JVMS
    /// §5.4.3.3 lookup from the constant-pool class, made lookup-only by the
    /// VM resolver (it never loads) -- or `0` when unknown: another kind, an
    /// unloaded or unresolvable owner, a scalar row, a hand-built fixture, or
    /// `CRATONVM_JIT_IR_SELF_COPY_BY_ID` off (the resolver fills it only
    /// under that switch).
    ///
    /// Not [`Self::declaring_class_id`], which names the class whose POOL
    /// spelled the call, and not [`Self::class_name`], which is the qualifier
    /// as written: `invokestatic Sub.f` with `Sub extends C` not hiding `f`
    /// resolves to `C.f`, and only this id says so
    /// ([`ir_self_copy_target_is_self`], JIT round 21 wave 16, lane
    /// jit-selfcopy; w15 review item 4).
    pub resolved_class_id: u32,
    /// The class id of `class_name` when the resolver SUBSTITUTED it for the
    /// constant-pool class (a private `invokevirtual` / `invokeinterface`
    /// pinned to its declaring class, `invoke_kind == 1`), else `0`. Copied
    /// into the interned `JitInvokeInfo::owner_class_id` by
    /// [`intern_inline_invoke_targets`], so the dispatch helper takes the
    /// class the substitution found instead of resolving the substituted name
    /// through the body's loader, the same as a top-level site's substituted
    /// owner (interpreter round i1 wave 17, lane L3; proposal
    /// `i14-L2-proposal-substitutions-keyed-by-call-site`, stage 1).
    pub owner_class_id: u32,
    /// `(compiled entry address, callee takes the VM context)` when this target
    /// could be bound to a raw `CALL` at plan time, `None` when it could not.
    ///
    /// This is what makes a call-carrying splice pay for itself. Measured
    /// 2026-08-18: emitting an admitted call through the blind
    /// `jit_invoke_dispatch` took the JUnit assertion chain from 47 to 163-266
    /// ns/iter, because the call it replaced was ALREADY direct-bound — a raw
    /// `CALL` to a compiled entry — and the helper resolves by name on every
    /// execution. Removing a ~4 ns frame does not pay for a ~175 ns downgrade.
    ///
    /// Filled by the same `callee_compiler` / `direct_callee_lookup` resolver
    /// the top-level `direct_calls` planning uses, so every one of its refusal
    /// gates (FJP blocklist, native shadow, callee exception table,
    /// `synchronized`, JVMS §5.5 static-init, indy trap, eager-chain depth /
    /// cycle / fan-out) applies here unchanged and in the same order.
    ///
    /// KEEP-ALIVE CONTRACT: an entry recorded here MUST reach
    /// `CompiledMethod::_direct_callee_entries`, which is what pins the callee
    /// artifact (`prepare_for_publication`) and what the invalidation reverse
    /// closure walks to evict a caller whose callee was withdrawn.
    /// `try_compile_inner` does that for every site, nested ones included; a
    /// baked address that skipped it is a use-after-free waiting for a tier-up.
    pub direct_entry: Option<(usize, bool)>,
}

/// One call inside a spliced body, resolved and interned for the emitter.
///
/// Two shapes, and the emitter prefers the first:
///
///  * `direct_entry != 0` — a raw `CALL` to the callee's compiled entry, the
///    same thing the top-level `direct_calls` path emits. `info_addr` is still
///    needed: a baked direct call has no dispatch-helper frame to recover its
///    arguments from when the callee deopts, so
///    `emit_inline_callee_deopt_check` reads them out of a service copy keyed
///    by this `JitInvokeInfo`.
///  * `direct_entry == 0` — the blind `jit_invoke_dispatch` helper. Only
///    reachable with `CRATONVM_JIT_INLINE_CALL_DISPATCH` on, because it is a
///    measured 3.5x pessimisation on an already-direct-bound chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ResolvedInlineInvoke {
    /// Bytecode pc in the CALLEE's own code, which is the space the emitter's
    /// inner walk indexes.
    pub callee_pc: usize,
    /// `*const JitInvokeInfo` as an address. Never 0 for a recorded site.
    pub info_addr: usize,
    /// Compiled callee entry for a direct `CALL`, or 0 for "dispatch".
    pub direct_entry: usize,
    /// Whether that entry expects the VM context as its first argument.
    pub direct_needs_context: bool,
    /// Operand slots the call consumes, receiver included.
    pub num_jit_args: usize,
    /// Descriptor return byte (`b'V'` for void).
    pub return_type: u8,
}

/// A call inside a spliced body that is spliced IN TURN rather than called.
///
/// `guard_class_id` is what makes this more than a recursion:
///
///  * `0` — the target is statically bound (`invokestatic` / `invokespecial`),
///    so there is exactly one body and the splice is unconditional.
///  * non-zero — the target is `invokevirtual` / `invokeinterface`, and this is
///    the receiver class the CALLEE's own profile says dominates that site. The
///    emitter guards the splice with an exact class-id compare and sends the
///    miss edge to the ordinary call, exactly as PGO-02 does one level up.
///
/// Why the callee's OWN profile is the right source, and why this needed no
/// re-keyed (caller pc, callee pc) profile after all: receiver types are
/// recorded by the interpreter against the bci of the method that is EXECUTING.
/// A call inside `objectsAreEqual` is therefore already profiled under
/// `objectsAreEqual`'s own `MethodKey` at its own bci — which is precisely the
/// (method, pc) pair a nested site names. The enclosing method's profile never
/// had this information and never could.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NestedInlineSite {
    /// Bytecode pc in the enclosing CALLEE's code.
    pub callee_pc: usize,
    /// Exact receiver class the splice is guarded on, or 0 for no guard.
    pub guard_class_id: u32,
    /// The body to splice.
    pub site: InlineSite,
    /// JIT round 19 wave 2 (lane splice2, SPLICE19-4,
    /// `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE`): a SECOND class's body for
    /// an IR-mode exact-receiver row (`guard_class_id != 0`) at the same
    /// `callee_pc`, its own `guard_class_id` the class it was resolved for.
    /// The builder tests it on the first class's miss edge (GS-1 one level
    /// down). `None` everywhere else; the VM's nested resolver fills it only
    /// under the switch, and the planner drops it when it does not admit it
    /// ([`ir_splice_strip_unadmitted_exact_nested`]).
    pub bimorphic_second: Option<Box<NestedInlineSite>>,
}

/// [`InlineSite::nested_sites`]: a copy-on-write list. The inline resolver's
/// memo hands out a clone of an answer for every repeat, and a plain
/// `Vec<NestedInlineSite>` made each clone copy the whole nested tree (about
/// 1.2% of a javac run's samples). Cloning this shares the list; the first
/// mutation of a shared list copies one level (its rows share their own lists
/// in turn). Reads and in-place edits go through [`Deref`](std::ops::Deref) /
/// [`DerefMut`](std::ops::DerefMut) as for the `Vec` it wraps.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NestedSites(pub(crate) std::sync::Arc<Vec<NestedInlineSite>>);

impl std::ops::Deref for NestedSites {
    type Target = Vec<NestedInlineSite>;

    fn deref(&self) -> &Vec<NestedInlineSite> {
        &self.0
    }
}

impl std::ops::DerefMut for NestedSites {
    fn deref_mut(&mut self) -> &mut Vec<NestedInlineSite> {
        std::sync::Arc::make_mut(&mut self.0)
    }
}

impl From<Vec<NestedInlineSite>> for NestedSites {
    fn from(rows: Vec<NestedInlineSite>) -> Self {
        Self(std::sync::Arc::new(rows))
    }
}

impl FromIterator<NestedInlineSite> for NestedSites {
    fn from_iter<I: IntoIterator<Item = NestedInlineSite>>(rows: I) -> Self {
        Self::from(rows.into_iter().collect::<Vec<_>>())
    }
}

impl IntoIterator for NestedSites {
    type Item = NestedInlineSite;
    type IntoIter = std::vec::IntoIter<NestedInlineSite>;

    fn into_iter(self) -> Self::IntoIter {
        std::sync::Arc::unwrap_or_clone(self.0).into_iter()
    }
}

impl<'a> IntoIterator for &'a NestedSites {
    type Item = &'a NestedInlineSite;
    type IntoIter = std::slice::Iter<'a, NestedInlineSite>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut NestedSites {
    type Item = &'a mut NestedInlineSite;
    type IntoIter = std::slice::IterMut<'a, NestedInlineSite>;

    fn into_iter(self) -> Self::IntoIter {
        std::sync::Arc::make_mut(&mut self.0).iter_mut()
    }
}

/// Resolved metadata for a method eligible for inlining at a specific call site.
///
/// `PartialEq`/`Debug` exist so an [`InlinePlan`] can CARRY the bodies a
/// speculative verdict resolved (see [`InlinePlan::speculative_sites`]) while
/// staying comparable and printable like the rest of the plan.
/// Sentinel `class_id` for an unloaded class in splice site tables (`ClassId(0)` is `java/lang/Object`).
pub const UNLOADED_SPLICE_CLASS_ID: u32 = u32::MAX;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InlineSite {
    /// Raw callee bytecode (padded with 2 sentinel bytes, like normal methods).
    pub callee_code: Vec<u8>,
    /// Original interpreter body when the spliced copy masks cold throw arms.
    /// Snapshot liveness must describe the bytecode a deopt will resume.
    pub callee_original_code: Option<Vec<u8>>,
    /// Round rx27 (lane matcher): the method's OWN exception table,
    /// `(start_pc, end_pc, handler_pc, catch_type)` body-relative (`catch_type`
    /// the class file's pool index, 0 catch-all), empty when it has none
    /// (the spliced copy may have its handlers stripped or masked). Its
    /// [`SpliceScopeSource`] row carries it with [`Self::callee_original_code`]
    /// (`splice_scope_sources`), and its chain-snapshot liveness is
    /// taken over the same pair (`spliced_body_codes`), so a stripped fence
    /// `finally` still keeps the locals it fences live.
    pub callee_original_handler_rows: Vec<(usize, usize, usize, u16)>,
    /// Actual bytecode length (excluding padding).
    pub callee_code_len: usize,
    /// Number of callee local variable slots.
    pub callee_max_locals: usize,
    /// Number of JVM stack slots consumed by arguments (including receiver for instance methods).
    pub callee_num_args: usize,
    /// Whether the callee is a static method.
    pub callee_is_static: bool,
    /// Return type tag (b'I', b'J', b'D', b'F', b'L', b'[', b'V').
    pub return_type: u8,
    /// Resolved field access in callee bytecode: (callee_pc, field_index, type_tag).
    pub field_info: Vec<(usize, usize, u8)>,
    /// Round 15 wave 3 (lane chain3, CHN15-2 producer): `(callee_pc,
    /// field_index, declaring class id)` of each [`Self::field_info`] row
    /// whose resolution named the field's DECLARING class with certainty,
    /// from the same resolution as that row. Rebased into
    /// `ir::IrInlineTables::field_decl_class` only where the pc and index
    /// match a `field_info` row. Empty = no identity (the index-only alias
    /// rule), which every hand-built fixture keeps.
    pub field_decl_info: Vec<(usize, usize, u32)>,
    /// Compact-layout metadata for resolved callee fields:
    /// (callee_pc, byte_offset_from_object_body, is_reference).
    /// Kept separate from `field_info` so legacy-layout compilation and
    /// consumers that only need the abstract slot index stay unchanged.
    pub compact_field_info: Vec<(usize, u32, bool)>,
    /// Resolved static field access: (callee_pc, class_id_raw, field_index, type_tag, is_volatile).
    pub static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    /// Resolved ldc constants: (callee_pc, i64_value).
    pub ldc_info: Vec<(usize, i64)>,
    /// Resolved ldc2_w constants: (callee_pc, i64_value).
    pub ldc2w_info: Vec<(usize, i64)>,
    /// Callee PCs, among [`Self::ldc_info`] and [`Self::ldc2w_info`], whose
    /// constant is a **float or double** rather than an int or a long.
    ///
    /// The two vectors above carry a bare `i64` because that is all the
    /// single-pass emitter needs: it materialises the bits and the surrounding
    /// bytecode decides how to read them. The IR builder cannot work that way —
    /// it must choose a NODE, `Op::ConstF` typed `Float`/`Double` or `Op::Const`
    /// typed `Int`/`Long`, at the moment it lowers the `ldc`, and it cannot
    /// infer the width from the opcode (`ldc` is polymorphic) or from admission
    /// (a method whose only FP is `ldc 1.5f` is admitted through the int
    /// clause). Its own `ldc_info`/`ldc2w_info` maps are therefore
    /// `(bits, is_fp)` pairs and always have been.
    ///
    /// That one missing bit was the whole of the `ir-splice-ldc` refusal: a
    /// callee containing ANY `ldc` was refused for inlining by the optimizing
    /// tier — not for a modelling reason, but because this struct dropped the
    /// tag on the floor. Measured on a four-callee probe, `ldc` alone refused
    /// two of the four (`x *= 0x9E3779B1`, `s * 1000003L` — a constant wider
    /// than `sipush` is unremarkable in real code).
    ///
    /// A `Vec` rather than a third parallel `(pc, value, flag)` vector so the
    /// two existing shapes and every consumer of them stay byte-identical; the
    /// lists are per-callee and short, so membership is a linear scan over a
    /// handful of entries at splice time only.
    pub ldc_fp_pcs: Vec<usize>,
    /// Whether the callee needs VM context (heap pointer).
    pub needs_heap: bool,
    /// Class name of the inlined callee (for invalidation tracking).
    pub class_name: String,
    /// `ClassId` of [`Self::class_name`], or `0` when the producer did not
    /// supply one (every hand-built test fixture).
    ///
    /// The resolver knows this id -- it is what it looked the body up by -- and
    /// used to drop it. That is why an inlined level in a stack walk carried
    /// only a NAME, and why `stackwalker::frame_class_ids_with_compiled` could
    /// not expand one: that walk answers in `ClassId`, takes no `ClassStore`,
    /// and resolving a JIT label by name to answer the JEP 403 deep-reflection
    /// gate would be a security-relevant GUESS. Carried here it is not a guess:
    /// it is the same id the splice's own invalidation dependency is recorded
    /// against.
    ///
    /// `0` is the "unknown" sentinel, matching
    /// [`NestedInlineSite::guard_class_id`]'s use of it, and a consumer must
    /// REFUSE on it rather than substitute anything.
    pub class_id: u32,
    /// Method name of the inlined callee.
    pub method_name: String,
    /// Descriptor of the inlined callee.
    pub descriptor: String,
    /// Callee PCs of `invokespecial` instructions the resolver PROVED are
    /// no-ops and may be elided: calls to `java/lang/Object.<init>()V` or to
    /// a super constructor whose body is exactly
    /// `aload_0; invokespecial Object.<init>; return` (the
    /// `is_elidable_construction` predicate). This is what makes CONSTRUCTOR
    /// bodies inlineable — every ctor starts with such a super call, which
    /// historically caused a blanket 0xb7 rejection in the inline resolver,
    /// so no constructor could ever inline and every `new C(args)` paid a
    /// full `jit_invoke_dispatch` round trip per allocation. The inline body
    /// emitter pops the receiver the preceding `aload_0` pushed and emits
    /// NOTHING for these PCs; any 0xb7 NOT in this list still bails.
    pub elided_invoke_pcs: Vec<usize>,
    /// Calls the callee body itself makes, keyed by CALLEE pc — see
    /// [`InlineInvokeTarget`]. Empty unless the resolver's call-splicing gate
    /// admitted the site, in which case every `invoke{virtual,static,interface}`
    /// in the body has an entry (a site with even one unresolvable invoke is
    /// refused whole, exactly like an unmodelled `ldc`).
    ///
    /// This is the RESOLVER's output. The emitter does not read it directly; it
    /// reads [`Self::resolved_invoke_infos`], which `try_compile_inner` derives
    /// from it.
    pub invoke_targets: Vec<(usize, InlineInvokeTarget)>,
    /// Static call pcs whose constant-pool owner was not loaded when resolved.
    /// They can only become precise interpreter exits, never blind calls.
    pub unloaded_static_call_pcs: Vec<usize>,
    /// The interned form of [`Self::invoke_targets`], filled by
    /// `try_compile_inner` immediately before backend emission and read by
    /// `try_emit_inline_body`'s invoke arm.
    ///
    /// Addresses are `usize` rather than raw pointers so the struct keeps its
    /// derived `Clone`/`Eq`/`Debug` and stays `Send`: an `InlineSite` is
    /// planning data that may be cloned into an `InlinePlan` and moved between
    /// threads, while the pointers are only ever dereferenced inside the one
    /// compile that interned them. Empty on every path that does not intern
    /// (planning, tests, and every compile with the gate off), and an empty
    /// vector makes the emitter's invoke arm bail exactly as it did before it
    /// existed.
    ///
    /// SAFETY CONTRACT: `info_addr`'s pointee lives in this compile's
    /// `_jit_invoke_infos` arena, which `try_compile_inner` moves into the
    /// `CompiledMethod` — so it outlives the emitted code, which bakes the
    /// address as an immediate. An `InlineSite` that escapes that compile
    /// (a cached plan) must not carry these; nothing repopulates them, and
    /// `try_compile_inner` overwrites the vector wholesale on every compile.
    pub resolved_invoke_infos: Vec<ResolvedInlineInvoke>,
    /// Calls the callee body makes that are themselves SPLICED rather than
    /// dispatched, keyed by CALLEE pc — the nesting step.
    ///
    /// A pc present here is emitted by recursing into `try_emit_inline_body`
    /// with the nested site; a pc present only in [`Self::invoke_targets`]
    /// takes the dispatch helper. Both are populated for a nested pc, so a
    /// nested splice that bails mid-body falls back to the ordinary call
    /// instead of failing the whole outer splice.
    ///
    /// Depth is bounded by the resolver (`MAX_INLINE_NEST_DEPTH`); this vector
    /// is empty at the deepest admitted level, which terminates the recursion.
    pub nested_sites: NestedSites,
    /// IR-tier only: resolved `new` sites in the callee body,
    /// `(callee_pc, class_id, num_fields, finalizable)`. `finalizable` is the
    /// VM's `has_finalizer` answer for the class (a `finalize()` override, a
    /// `java.lang.ref` reference class, or not initialised yet); such a class
    /// joins `ir::Graph::finalizable_new_classes`, which keeps its allocation
    /// off scalar replacement (JIT round 20 wave 12, lane hFin).
    ///
    /// Empty for every site the single-pass emitter plans — its resolver
    /// refuses a body containing `new` outright, because `try_emit_inline_body`
    /// has no arm for one. `IrBuilder` does, and it is the arm that matters: an
    /// accessor that allocates is exactly the body escape analysis has to see
    /// inside its caller. Filled by `resolve_ir_inline_site` only, and a
    /// `Deferred` target refuses the whole splice rather than leaving a row out
    /// — a missing row bails the METHOD, not the site.
    pub ir_new_info: Vec<(usize, u32, usize, bool)>,
    /// JIT round 25 wave 10 (W10-6, [`ir::ir_splice_anewarray_enabled`]):
    /// `(callee_pc, component_class_id)` for each `anewarray` in the callee
    /// body, resolved by the same resolver as the caller's own `anewarray`
    /// rows. Filled by `resolve_ir_inline_site` only, and only under the
    /// switch; an unresolved component refuses the whole splice (a missing
    /// row would bail the METHOD at the builder's `0xbd` arm).
    pub ir_anewarray_info: Vec<(usize, u32)>,
    /// Resolved `checkcast` / `instanceof` sites in the callee body:
    /// `(callee_pc, target_class_id, target_class_name, is_checkcast)`.
    ///
    /// Filled by `resolve_ir_inline_site` only, and an UNRESOLVED target
    /// refuses the whole splice rather than leaving a row out -- the same
    /// trade, and the same sentence, as [`Self::ir_new_info`]'s: a missing row
    /// bails the METHOD, so admitting the body without one would cost the
    /// CALLER its optimizing compile over a callee it merely wanted inlined.
    ///
    /// The builder's `0xc0`/`0xc1` arms look gentler than that -- a missing row
    /// reaches `plant_uncommon_trap` rather than `ir_build_bail`. By default it
    /// is not: `TrapCause::UnresolvedTypeCheck` is gated off
    /// (`ir_unresolved_class_trap_enabled`), so the plant refuses and the arm
    /// bails after all. With that gate ON the trap does fire, and inside a
    /// splice it deopts to re-execute the invoke on every call. Refusing at
    /// resolution is the answer that does not depend on which way that flag is
    /// set.
    ///
    /// The NAME rides along rather than being re-derived downstream: it is read
    /// out of the CALLEE's constant pool, which is the only pool that can
    /// answer, and the same lookup that produced `target_class_id`. Interning
    /// happens in `append_ir_inline_site` because that is where the
    /// combined-buffer pc is known.
    pub ir_typecheck_info: Vec<(usize, u32, String, bool)>,
    /// JIT r23 w2c close: the callee pcs of [`Self::ir_typecheck_info`] whose
    /// resolved target the VM proved a `final`, non-interface class (the
    /// `CompileRequest::class_is_final_resolver` verdict). `append_ir_inline_site`
    /// interns those under the final identity
    /// ([`intern_typecheck_target_with_finality`]), as the compiling method's
    /// own sites are, so a spliced `instanceof` of a final user class answers
    /// its miss inline and the checkcast predicate can plan a spliced cast.
    /// Empty: every spliced site interned as before (not final).
    pub ir_typecheck_final: Vec<usize>,
    /// Callee pcs of the `getfield`s in this body whose field is `volatile`
    /// (round 9 wave 10, irl10; landed by wave 10b, wire10b). The IR tier
    /// builds each as an ordered `Op::Load`
    /// (`ir::IrBuilder::add_spliced_volatile_field_pcs`); the single-pass
    /// inliner reads it with a plain load, which x86-64 orders as an acquire.
    /// Never a `putfield`: the resolver refuses those bodies -- except an
    /// optimizing-tier site under `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`
    /// (`ir::ir_splice_volatile_store_enabled`, JIT round 21 wave 13, lane
    /// cf), whose volatile `putfield` pcs are listed here too and built as a
    /// fenced ordered `Op::Store`.
    pub volatile_field_pcs: Vec<usize>,
    /// nio21 wave 2 (lane L3): callee pcs of the `getfield`s in this body
    /// whose resolved field is declared `final` (`ACC_FINAL`). The IR tier
    /// rebases them into `ir::IrInlineTables::final_field_pcs`, where the
    /// G14-1 memo may forward such a read under
    /// `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD`; the single-pass inliner ignores
    /// it. Empty is always safe (no forwarding).
    pub final_field_pcs: Vec<usize>,
    /// nio21 wave 7 (lane L4, W5L4-3): callee pcs of the `getfield`s in this
    /// body whose resolved field is one the VM knows holds a `byte[]` or null
    /// (descriptor `[B` of a bootstrap `java.*` class that declares exactly
    /// one field of that name: `java/nio/ByteBuffer.hb`). The IR tier rebases
    /// them into `ir::IrInlineTables::byte_array_field_pcs`, and the scoped
    /// fold's heap arm skips the primitive-array screen and the element-size
    /// read for a base loaded there (`ir::Graph::byte_array_nodes`), under
    /// `CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE`. Empty is always safe (every
    /// base is screened).
    pub byte_array_field_pcs: Vec<usize>,
    /// JIT r20 wave 18 (lane xLib, THROW20-4; the VM's
    /// `CRATONVM_JIT_IR_REFERENT_SPLICE`): callee pcs of the `getfield`s of
    /// `java.lang.ref.Reference.referent` in an OPTIMIZING splice (the body of
    /// `Reference.get()`). Each has its `field_info` row and deliberately no
    /// compact row and no `field_decl_info` row. The IR tier rebases them into
    /// `ir::IrInlineTables::referent_load_pcs` (`ir::Graph::referent_load_pcs`),
    /// where `ir_lower` sends the load to the checked `jit_getfield` helper --
    /// the only compiled load that carries the referent's SATB keep-alive --
    /// and no field-forwarding pass can name it (no identity). The single-pass
    /// resolver never fills it (it refuses such a body). Empty is the old
    /// behaviour.
    pub referent_load_pcs: Vec<usize>,
    /// Round 14 wave 2 (lane syncsplice, SR-1): the callee is `synchronized`
    /// (`ACC_SYNCHRONIZED`), so its body is correct only with its monitor held
    /// -- the receiver for an instance method, the class mirror for a static
    /// one (JVMS 2.11.10).
    ///
    /// Only the optimizing tier's constant-pool resolution at nesting depth 0
    /// ever answers such a body (`resolve_ir_inline_site`, under
    /// [`ir::ir_sync_splice_enabled`]); every other resolver still refuses it.
    /// The IR planner splices one ONLY as a synchronized splice
    /// (`ir_sync_splice_site_refusal`, `IrBuilder::add_sync_splice_site`),
    /// which takes the monitor around the body; a site it cannot admit that
    /// way stays its call. `false` for every body built before this existed.
    pub callee_is_synchronized: bool,
    /// Round 14 wave 7 (lane sync7, SS8-5 through proposal S6-1): the body's
    /// `ldc`/`ldc_w` of a `CONSTANT_Class` entry, `(callee_pc, class id)`, the
    /// id being the class the VM resolved the entry to (already loaded; no
    /// loading run). Filled only by the optimizing tier's resolution of a
    /// `static synchronized` body at depth 0 for an entry naming the callee's
    /// OWN class, under [`ir::ir_sync_splice_self_class_holds_lock_enabled`];
    /// every other resolution still refuses a body with such an `ldc`
    /// (`ldc-non-numeric-constant`). The planner admits a row only as the
    /// first half of `holdsLock(C.class)` inside that body's own window
    /// (`ir_sync_splice_body_scan`), and `append_ir_inline_site` refuses any
    /// other, so no spliced `ConstClass` is ever built from one. Empty for
    /// every other body.
    pub ir_ldc_class_info: Vec<(usize, u32)>,
    /// JIT r20 wave 19 (lane zLdc, THROW20-4 `date`,
    /// [`ir::ir_splice_ref_ldc_enabled`]): the body's `ldc`/`ldc_w` of a
    /// `CONSTANT_String` or `CONSTANT_Class` entry, `(callee_pc, holder class
    /// id, cp index, is_class, slot address)`: the holder is the class whose
    /// pool the body's bytecode indexes (the declaring class), and the slot is
    /// the VM's GC-maintained word holding the RESOLVED constant -- never `0`:
    /// a site whose constant is not resolved yet (or has no slot) keeps the
    /// body refused (`ldc-non-numeric-constant`), so no resolution side
    /// effect (loading, access check, `LinkageError`, interning) moves into
    /// compiled code. Filled only by the optimizing tier's resolution;
    /// `append_ir_inline_site` rebases the rows into
    /// `ir::IrInlineTables::ldc_ref_string` / `ldc_ref_class` /
    /// `ldc_ref_slots`. Empty for every other body.
    pub ir_ldc_ref_rows: Vec<(usize, u32, u16, bool, usize)>,
    /// JIT r20 wave 19 (lane zLdc): `(callee_pc, class id)` of the body's
    /// `invokevirtual Object.getClass()` (resolved to the method
    /// `java/lang/Object` declares) followed by an [`Self::ir_ldc_ref_rows`]
    /// class row naming `C` and an `if_acmpeq`/`if_acmpne`, for a `C` whose
    /// `getClass()` reports only itself. Rebased into
    /// `ir::IrInlineTables::splice_getclass_compares`. Empty for every other
    /// body.
    pub ir_getclass_compares: Vec<(usize, u32)>,
    /// Round nio21 wave 4 (lane L2): the body's exception table, when the
    /// optimizing tier's resolver admitted it under
    /// [`ir::ir_splice_callee_handlers_enabled`]
    /// ([`ir::splice_callee_handler_plan`]): `(start_pc, end_pc, handler_pc,
    /// catch-type name, declaring class id)` in table order, callee pcs, the
    /// name resolved through the DECLARING class's constant pool (never a
    /// catch-all). `callee_code` is then the copy with every handler masked.
    /// Empty for every other body (a body with no table, or the javac fence
    /// `finally` the resolver strips).
    pub ir_handler_rows: Vec<(usize, usize, usize, String, u32)>,
    /// nio21 wave 11, lane L3 (W10L3-1, [`ir::ir_splice_indy_enabled`]): the
    /// body's `invokedynamic`s, `(callee_pc, argument entries, return tag,
    /// bridge site)`, when the optimizing tier's resolver admitted every one
    /// as a CONSTANT-WORD lambda site: no arguments, a reference result, the
    /// instruction (keyed by the callee's own method and bci, exactly as the
    /// interpreter keys it) linked and its singleton published on the VM's
    /// site. `append_ir_inline_site` rebases them into
    /// `ir::IrInlineTables::indy_trap_sites` / `indy_bridge_sites`. Empty for
    /// every other body (the resolver refuses any other `invokedynamic`).
    pub ir_indy_rows: Vec<(usize, usize, u8, usize)>,
    /// JIT round 20 wave 19 (lane ySplice, SPLICE20-1,
    /// [`ir::ir_splice_indy_capture_enabled`]): the body's linked CAPTURING
    /// lambda sites, `(callee_pc, proxy class id, capture tags)` -- the VM's
    /// `(proxy class, capture count)` fact for the instruction (keyed by the
    /// callee's own method and bci) and the descriptor's
    /// [`indy_arg_type_tags`], one per capture. `append_ir_inline_site`
    /// rebases each into `ir::IrInlineTables::indy_capture_news` and
    /// `indy_trap_sites`; the builder allocates the proxy inline
    /// (`ir::IrBuilder::try_indy_capture_new`). Disjoint from
    /// [`Self::ir_indy_rows`] (no pc is in both). Empty for every other body.
    pub ir_indy_capture_rows: Vec<(usize, u32, Vec<u8>)>,
    /// JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): `(callee_pc, bridge
    /// site)` of each [`Self::ir_indy_capture_rows`] row -- the interned site
    /// the VM keyed the row's fact by, so the compile door can ask
    /// `CompileRequest::indy_lambda_impl_resolver` which static impl the proxy
    /// calls ([`ir_graft_spliced_lambda_impls`]). Empty for every other body;
    /// an empty vector only means no capturing SAM call in the body is planned.
    pub ir_indy_capture_sites: Vec<(usize, usize)>,
    /// JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2,
    /// [`ir_lambda_impl_in_splice_enabled`]): filled by the compile door only,
    /// never by a resolver. `(SAM call callee_pc, invokedynamic callee_pc,
    /// capture count, impl parameter count)` of each `invokeinterface` in this
    /// body whose receiver straight-line code proves is the value of the body's
    /// own lambda `invokedynamic`, and whose lambda's static impl was resolved
    /// as a [`NestedInlineSite`] at the same pc. `append_ir_inline_site`
    /// appends that impl body like any nested one but never refuses the
    /// enclosing body over it (a refused impl leaves the SAM call the inline
    /// cache it was), and registers a row only for an appended body, rebased
    /// into `ir::IrInlineTables::lambda_impl_sites`. Empty = no such call.
    pub ir_lambda_impl_rows: Vec<(usize, usize, usize, usize)>,
    /// JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2,
    /// [`ir_splice_callee_profile_seed_enabled`]): `(callee_pc, receiver
    /// counts)` of this body's `invokevirtual` / `invokeinterface` sites, read
    /// from the CALLEE's own method profile (keyed by [`Self::class_id`],
    /// [`Self::method_name`], [`Self::descriptor`]) when the VM resolved it.
    /// The optimizing planner seeds the inline cache of each such site that
    /// survives the splice from its row, as it seeds the caller's own sites
    /// from the caller's profile. Empty with the switch off, for every
    /// hand-built fixture and for a body with no profiled virtual site; an
    /// empty vector is exactly the unseeded cache the splice always had.
    pub callee_receiver_profile: Vec<(usize, profile::ReceiverCounts)>,
    /// JIT round 23 wave 4 (lane L1, CF23-2; [`ir_splice_never_taken_enabled`]):
    /// this body's conditional forward branches whose TAKEN edge the CALLEE's
    /// own method profile never saw ([`splice_never_taken_pcs`]), as callee
    /// pcs. The optimizing builder may prune them inside the splice as
    /// chain-framed guards (`IrBuilder::prune_never_taken_branch`). Empty with
    /// the switch off and for every hand-built fixture: no prune, as before.
    pub callee_never_taken_pcs: Vec<usize>,
    /// Conditional forward branches whose fall-through edge the callee's own
    /// method profile never saw, as callee pcs.
    pub callee_always_taken_pcs: Vec<usize>,
    /// `(switch_pc, target_pc)` callee-pc edges of cold switch arms.
    pub callee_pruned_switch_edges: Vec<(usize, usize)>,
    /// JIT round 23 wave 8 (lane W8-L2, CF-W8-3;
    /// `ir_evidence::cold_blind_dispatch_weight_enabled`): `(start, end,
    /// permille)` callee-pc ranges only a RARELY taken branch arm reaches, by
    /// the CALLEE's own profile ([`splice_cold_arm_ranges`]). Used only to
    /// price an in-splice blind dispatch for the acceptance gate; never changes
    /// what is built. Empty with the switch off and for hand-built fixtures.
    pub callee_cold_arm_ranges: Vec<(usize, usize, u16)>,
    /// JIT round 23 wave 10 (lane L2, CF-W10-2;
    /// [`ir_splice_cold_nested_hot_path_enabled`]): the callee pcs on this
    /// body's profiled dominant path ([`splice_hot_path_pcs`]). A nested call
    /// at one of them is not "cold" for [`ir_splice_prune_cold_nested`]. Empty
    /// with the switch off and for hand-built fixtures: the static rule alone.
    pub callee_hot_path_pcs: Vec<usize>,
    /// JIT r25 wave 8 (W8-5, `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`): this
    /// body's interpreted invocation count when the VM resolved it, the
    /// denominator of the frequency of each of its call sites
    /// (`callee_receiver_profile` samples / this). `0` -- unknown, every
    /// hand-built fixture and the switch off -- reads as "no evidence"
    /// ([`r25_w7b_chain_splice::strip_chain_tree`]'s cost rule then takes a
    /// site as running once per entry).
    pub callee_invocations: u32,
    /// Interpreter round i6 wave 122, lane L2 (step 3 of
    /// `i37-L6-proposal-hidden-method-bit-at-link-time`,
    /// `CRATONVM_HIDDEN_METHOD_BITS`): HotSpot's `Method::is_hidden` for this
    /// callee, as the VM's resolver decided it from the class store (a hidden
    /// class, or `@Hidden` in a privileged class). The single-pass emitter
    /// stamps it into every inline-frame level it records for this splice
    /// ([`x64::InlineFrameLevel::hidden`]), so a stack capture reads the bit
    /// instead of judging the level's label. `None` -- the default, every
    /// hand-built fixture, the switch off -- means "not decided": the
    /// capture judges the label as before.
    pub hidden: Option<bool>,
    /// j27 wave 5 (lane PROXY, `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`,
    /// opt-in): for a body the VM's profile-receiver resolver answered at an
    /// `invokeinterface` site, the loaded classes whose exact objects select
    /// this same body there, the profile's class first
    /// (`ClassRealm::cha_interface_class_set`). The planner hands it to
    /// `IrBuilder::add_cha_class_set_site`. Empty -- the default, every other
    /// resolver, the switch off -- plans exactly as before.
    pub cha_class_set: Vec<u32>,
    /// GC-maintained class mirror slot of this callee's declaring class (0 if
    /// ignored by security stack walk or unminted).
    pub ir_caller_mirror_slot: usize,
    /// True if `ignored_by_security_stack_walk` skips this spliced frame for
    /// `Reflection.getCallerClass()`.
    pub ir_security_walk_ignored: bool,
}

/// How many levels of splice-inside-a-splice the OPTIMIZING tier's resolver
/// will plan — deeper than [`MAX_INLINE_NEST_DEPTH`], and for a reason the
/// single-pass number does not have.
///
/// A partly-inlined allocation chain is worth nothing: the moment the object is
/// passed to a call the graph cannot see through, it escapes, and escape
/// analysis reports `scalar-replaced 0/N` exactly as it did with no inlining at
/// all. kfusion's own chain is four levels before the allocation's last use is
/// visible (`VolumeShort2.get` → `loadFromArray` → `Short2.<init>()V` →
/// `Short2.<init>([S)V`), so a budget of three would have stopped one short and
/// measured as no change.
pub const MAX_IR_INLINE_NEST_DEPTH: usize = 6;

/// The optimizing tier's splice nesting depth for this compile:
/// `CRATONVM_JIT_IR_INLINE_MAX_NEST` (netty compression wall, 2026-10-14),
/// clamped to `1..=12`, else [`MAX_IR_INLINE_NEST_DEPTH`]. Read per call.
///
/// JIT round 21 wave 2 (lane splice): and, under
/// [`ir_inline_max_nest_frame_clamp_enabled`] (default ON), to what the
/// BUILDER can open. A resolution at nesting depth `d` is the `d + 1`-th
/// splice frame, the resolver nests while `d + 1 < depth`, and the leaf tier
/// (`ir::ir_splice_leaf_past_depth_enabled`, default ON) one level more, so a
/// chain plans `depth + 1` frames; `IrBuilder::begin_splice` refuses the
/// `ir::MAX_IR_SPLICE_DEPTH + 1`-th (8) by refusing the whole BUILD, whose
/// rebuild then drops the chain's OUTERMOST site -- so a knob of 8..=12 lost
/// every deep chain it was set to keep (a call where 7 frames had fit).
pub fn ir_inline_max_nest_depth() -> usize {
    ir_inline_max_nest_depth_in(true)
}

/// [`ir_inline_max_nest_depth`] for a resolution inside (`chain_tree`) or
/// outside a dispatch-chain splice tree: the W10-3 raise of the unset knob
/// holds in a chain tree only, unless
/// [`r25_w7b_chain_splice::chain_depth_scoped_enabled`] is off.
pub fn ir_inline_max_nest_depth_in(chain_tree: bool) -> usize {
    let depth = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        // JIT r25 wave 10 (W10-3): under the chain switch the unset knob is
        // the deepest the builder opens (`r25_w7b_chain_splice::default_nest_depth_in`).
        .map_or_else(
            || r25_w7b_chain_splice::default_nest_depth_in(chain_tree),
            |n| n.clamp(1, 12),
        );
    if ir_inline_max_nest_frame_clamp_enabled() {
        depth.min(ir_inline_max_nest_frame_cap(
            ir::ir_splice_leaf_past_depth_enabled(),
        ))
    } else {
        depth
    }
}

/// The largest resolver nesting depth whose deepest chain still fits the
/// builder's [`ir::MAX_IR_SPLICE_DEPTH`] frames: one less when the leaf tier
/// adds a level past the cap. See [`ir_inline_max_nest_depth`].
pub(crate) fn ir_inline_max_nest_frame_cap(leaf_past_depth: bool) -> usize {
    // Round rx27 wave 4: [`ir::ir_max_splice_depth`] is 12 under
    // `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` (`rx27_deep_spine`).
    ir::ir_max_splice_depth() - usize::from(leaf_past_depth)
}

/// `CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP` (JIT round 21 wave 2, lane
/// splice; planning defect, **default ON**, `=0` restores the unclamped knob):
/// [`ir_inline_max_nest_depth`] never plans a chain deeper than the builder
/// opens. Changes nothing at the default depth (6, plus the leaf level = 7
/// frames). Read per call, like the knob.
pub fn ir_inline_max_nest_frame_clamp_enabled() -> bool {
    true
}

/// How many levels of splice-inside-a-splice the resolver will plan.
///
/// `1` reproduces the pre-nesting behaviour exactly (an outer body may be
/// spliced; nothing inside it is). Each extra level multiplies the planning
/// work and the emitted code, and the per-level expansion is already charged
/// against the same `inline_site_expansion_cost` budget the outer site pays,
/// so the ceiling is about bounding RESOLUTION cost, not code size.
///
/// Three levels is what the JUnit assert chain needs to collapse
/// (`assertEquals(int,int)` -> `assertEquals(Object,Object)` ->
/// `objectsAreEqual`), which is the shape that motivated nesting.
pub const MAX_INLINE_NEST_DEPTH: usize = 3;

/// Whether the optimizing tier may splice callee bodies into the graph it
/// builds. `CRATONVM_JIT_IR_INLINE=1`.
///
/// Opt-in, and the shipped default is OFF — but NOT for the reason this comment
/// used to give. It said the allocation is "only HALF deleted until array
/// scalar replacement lands beside it"; array scalar replacement landed on
/// 2026-08-27 and the allocation is fully deleted now (`volume` 478 ->
/// 43.9 ns/voxel, converging on its own no-wrapper control at 39.2; see
/// `per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`).
///
/// What keeps it off is no longer the pricing — the 2026-08-28 gauntlet soak
/// (`ir-inline-gauntlet-soak-20260828.md`) did that,
/// and the trades came out in the flag's favour: 11 030 methods spliced across
/// 200 netty classes, 8% faster on a serial netty slice and 15-26% on
/// hibernate, with the sharded run's apparent +18% traced to contention rather
/// than to compile cost.
///
/// What kept it off was a CORRECTNESS regression the soak found — a 3-byte
/// out-of-bounds read through a spliced accessor raising `InternalError`
/// instead of `IndexOutOfBoundsException`. **That is FIXED** (2026-08-28,
/// `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`):
/// `lower_inner_with_array_list` was passing `Lowerer::new` an empty
/// `spliced_ranges`, so every deopt inside a relocated body recorded a bci
/// the method does not have. `DuplicatedByteBufTest` is `ok=416 failed=0`
/// with the flag on, 3 reps, matching the flag-off arm.
///
/// **DEFAULT ON since 2026-09-06.** `CRATONVM_JIT_IR_INLINE=0` is the kill
/// switch and restores the pre-flip behaviour exactly (no callee body is
/// spliced, at any site, at any nesting depth).
///
/// The decision this comment used to defer, taken, and the reasoning written
/// down rather than left to be inferred:
///
/// * **The case FOR is the only positive throughput measurement anywhere in
///   this tier's record.** The 2026-08-28 gauntlet soak spliced 11,030 methods
///   across 200 netty classes and measured 8% on a serial netty slice and
///   15-26% on hibernate. Every other optimizing-tier switch this file
///   documents measured zero or near-zero on a real workload; this one did not.
/// * **The blocker was a correctness bug and it is fixed.** A spliced accessor
///   turned a 3-byte out-of-bounds read into `InternalError` because
///   `lower_inner_with_array_list` passed `Lowerer::new` an empty `spliced_ranges`,
///   so every deopt inside a relocated body recorded a bci the method does not
///   have. `DuplicatedByteBufTest` is `ok=416 failed=0` with the flag on.
/// * **It is what makes the rest of this tier worth anything.** An optimizer
///   that cannot see across a call boundary is an optimizer looking at accessor
///   bodies. Escape analysis in particular reports `scalar-replaced 0/N`
///   whenever an allocation reaches a call the graph cannot see through, which
///   on real code is almost always -- and it is why `MAX_IR_INLINE_NEST_DEPTH`
///   is 6 rather than 3.
///
/// **What is owed, and is not a reason to keep it off.** The soak's numbers
/// were taken before the deopt-bci fix; they should be re-run on a current
/// binary rather than inherited. And the designated differential gate cannot
/// see this flag at all -- it splices zero times in `ir_vs_singlepass`, which
/// has no VM to supply callee bodies -- so the evidence that this is correct is
/// the gauntlet soak and the regression suite, not that gate. A reader chasing
/// an unexplained miscompile in a spliced body should reach for `=0` first,
/// which is the whole reason the switch stays.
pub fn ir_inline_enabled() -> bool {
    // Read PER CALL, not cached in a `OnceLock`, and that is deliberate: this
    // is the one JIT gate a test arranges with `flags::with_thread_overrides`,
    // because `ir_inline_flag_changes_the_emitted_body` compiles the SAME
    // method twice in one process with the flag on and then off. Caching it
    // made the second arm read the first arm's answer, the two bodies came out
    // identical, and the test that exists to prove this harness can see the
    // inliner at all failed — which is exactly the vacuity
    // `ir-inline-gauntlet-soak-20260828` recorded.
    //
    // A compile is not a hot path; one `getenv` per compile costs nothing worth
    // trading a testable switch for.
    true
}

/// Total appended bytecode one compile may splice, across every site and every
/// nesting level.
///
/// Not a code-size budget — it bounds the WALK. Every appended byte is bytecode
/// `IrBuilder` visits and turns into nodes, and the node count is what
/// `ir_optimize`'s GVN and the escape-analysis connection graph scale in. Four
/// times [`MAX_INLINE_BYTECODE_SIZE`] is room for a full accessor chain at
/// several call sites without letting a resolver defect expand a method without
/// limit.
pub(crate) const IR_INLINE_MAX_TOTAL_BYTES: usize = 16 * MAX_INLINE_BYTECODE_SIZE;

/// The splice budget a production compile starts from:
/// `CRATONVM_JIT_IR_INLINE_BUDGET` (netty compression wall, 2026-10-14),
/// clamped to `IR_INLINE_MAX_TOTAL_BYTES..=8 * IR_INLINE_MAX_TOTAL_BYTES`,
/// else [`IR_INLINE_MAX_TOTAL_BYTES`]. Read per compile.
pub(crate) fn ir_inline_total_budget() -> usize {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(IR_INLINE_MAX_TOTAL_BYTES, |n| {
            n.clamp(IR_INLINE_MAX_TOTAL_BYTES, 8 * IR_INLINE_MAX_TOTAL_BYTES)
        })
}

/// Distinct caller call sites one compile may splice at.
pub(crate) const IR_INLINE_MAX_SITES: usize = 24;

/// Does this callee body have more than one `return` instruction (round 11
/// wave 11, lane `calls`)? Walked with the shared decoder, so an operand byte
/// that happens to equal a return opcode is never counted. Unreachable returns
/// count too: this is the admission question, and the resolver counted them
/// the same way.
pub(crate) fn ir_splice_body_has_several_returns(code: &[u8], code_len: usize) -> bool {
    let len = code_len.min(code.len());
    let mut returns = 0usize;
    let mut pc = 0usize;
    while pc < len {
        if matches!(code.get(pc), Some(0xac..=0xb1)) {
            returns += 1;
            if returns > 1 {
                return true;
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}

fn ir_splice_site_has_several_returns(site: &InlineSite) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    if site.callee_always_taken_pcs.is_empty()
        && site.callee_never_taken_pcs.is_empty()
        && site.callee_pruned_switch_edges.is_empty()
    {
        return ir_splice_body_has_several_returns(code, len);
    }
    let Some((live, _)) = splice_live_pcs_without_pruned_and_switch(
        code,
        len,
        &site.callee_always_taken_pcs,
        &site.callee_never_taken_pcs,
        &site.callee_pruned_switch_edges,
    ) else {
        return ir_splice_body_has_several_returns(code, len);
    };
    let mut returns = 0usize;
    let mut pc = 0usize;
    while pc < len {
        if live.get(pc).copied().unwrap_or(false) && matches!(code.get(pc), Some(0xac..=0xb1)) {
            returns += 1;
            if returns > 1 {
                return true;
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}

/// Round 14 wave 2 (lane syncsplice, SR-1): why the `synchronized` body `site`
/// may NOT be spliced as a synchronized splice (`ir::SyncSpliceWindow`), or
/// `None` when it may. A refused site keeps its call.
///
/// `top_level`: the site is one of the compiling method's own, resolved for
/// its constant-pool class (not an exact-receiver or profile body), in a
/// method with no monitor bytecode of its own unless
/// `CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION` admits one (SS-3). `row_admits`: an instance callee (which locks its receiver), or a static
/// one whose site carries the caller-held route's row
/// (`ir::IrBuilder::sync_splice_row_admits`), from which the splice takes the
/// class mirror. Trap-freedom is [`ir_sync_splice_body_scan`]'s, on the
/// bytecode; the builder and `ir_lower` check the graph after it.
pub(crate) fn ir_sync_splice_site_refusal(
    site: &InlineSite,
    top_level: bool,
    row_admits: bool,
) -> Option<&'static str> {
    if !ir::ir_sync_splice_enabled() {
        return Some("ir-sync-splice-off");
    }
    if !site.callee_is_synchronized {
        return Some("ir-sync-splice-not-synchronized");
    }
    if !top_level {
        return Some("ir-sync-splice-not-top-level");
    }
    if !row_admits {
        return Some("ir-sync-splice-no-caller-held-row");
    }
    ir_sync_splice_body_scan(site).err()
}



/// The class-mirror row a `static synchronized` splice may lock, from the
/// VM's `sync_direct_lookup` answer `t` for the site's Methodref: the static
/// row's shape `(holder, class cp index, mirror slot)`, `holder` being the
/// compiling method's declaring class (whose pool holds the index). The VM
/// answers a static target -- with `entry == 0` when only the callee's BODY
/// was refused (no published wrapped body: a self-locking one, or none) --
/// only for a method the Methodref's own class declares, initialised, so the
/// mirror is the callee's monitor (JVMS 2.11.10). `None` for an instance
/// answer or an unusable index.
pub(crate) fn ir_sync_splice_static_mirror(
    holder_class_id: u32,
    t: &crate::compile_request::SyncDirectTarget,
) -> Option<(u32, u16, usize)> {
    (!t.instance && holder_class_id != 0 && t.class_cp_idx != 0).then_some((
        holder_class_id,
        t.class_cp_idx,
        t.mirror_slot,
    ))
}



/// SS-5: may the planner skip its store fence for `site`? Only for a
/// synchronized body the splice scan admits, and only while the builder admits
/// committed stores at all (`committed_store_admitted`; with
/// `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE=0` it refuses the store itself,
/// synchronized splice or not).
pub(crate) fn ir_sync_splice_skips_store_fence(site: &InlineSite, committed_store_admitted: bool) -> bool {
    committed_store_admitted
        && site.callee_is_synchronized
        && ir::ir_sync_splice_enabled()
        && ir_sync_splice_body_scan(site).is_ok()
}

