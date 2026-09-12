# On-stack replacement: what exists, what this change adds, what it refuses

Scope: the P1 item *"Add on-stack replacement"* of
the C2 review.

> Build HIR from interpreter state at a loop BCI, validate local/stack types,
> enter compiled code, and support deoptimization back to the same loop state.

Acceptance criterion under audit:

> Long-running loops tier up without restarting and survive forced deopt.

CratonVM has had a working OSR transition for a long time. What it did **not**
have is the middle clause of the item — *validate local/stack types* — and,
consequently, it could not honour "survive forced deopt" without occasionally
re-running iterations the compiled body had already committed. This document
records the inventory, the admission check that was added, and the two edits
that are needed outside `jit/src/lib.rs`.

---

## 1. What already existed

Everything in this table predates this change and is **reused, not replaced**.

| Piece | Where | What it does |
| --- | --- | --- |
> **Location note, 2026-09-12.** Line numbers in this file went stale as the
> files grew, so the tables cite symbols. The VM half moved out of
> `vm/src/runtime/interpreter/invoke.rs`: `try_osr` and `compile_osr_artifact`
> are in `vm/src/runtime/interpreter/jit_bridge.rs`, and the exit transfers are
> in `vm/src/runtime/interpreter/deopt_resume.rs`.

| Piece | Where | What it does |
| --- | --- | --- |
| Per-bci entry table | `CompiledMethod::osr_pc_to_native` (`jit/src/lib.rs`), built from `x64::Compiler::osr_entry_native` | bci → native offset. `-1` for a pc strictly inside a LICM-hoisted body, whose preheader an entry there would skip. Points *before* a hoisted preheader, unlike the branch-patch `pc_to_native`. |
| Entry admission (structural) | `CompiledMethod::can_osr_enter` / `can_osr_enter_with` (`jit/src/lib.rs`) | "is there a native offset here, and is the dead-local mask acceptable". Under `CRATONVM_JIT_OSR_SINGLE_PC=1` (a default-off diagnosis lever) it also refuses any pc other than `osr_compiled_entry_pc`. |
| The machine transition | `CompiledMethod::osr_enter` → `osr_trampoline` → `emit_osr_trampoline` (`jit/src/lib.rs`) | builds the single-pass frame from outside and jumps to the entry offset; see §3, *The machine transition* |
| Coalesced-register hazard | `CompiledMethod::osr_dead_mask` + `osr_dead_local_entry_allowed` (`jit/src/lib.rs`) | names the dead locals that share a register with a *live* one, so the trampoline skips seeding them. `CRATONVM_JIT_OSR_DEAD_LOCALS=0` restores the historical blanket refusal. |
| Negative memo | `is_osr_entry_rejected` / `mark_osr_entry_rejected` (`jit/src/lib.rs`) | per-`(method, entry_pc)`, so a permanent refusal costs one pipeline run, not one per back-edge |
| Tiering trigger | `try_osr_with_backoff` (`vm/src/runtime/interpreter.rs`), throttled by `Frame::should_try_osr`, calling `TieredCompilationManager::request_osr` (`jit/src/tiered.rs`) | a `CompilationTask` carrying `osr_bci`. `TieredCompilationManager::on_backedge` also exists, but only tests call it. |
| The OSR compile | `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`); the optimizing door uses `compile_optimizing_artifact` | compiles an entry-pc-independent artifact into a separate OSR cache, flagged `compiled_via_osr` |
| The call site | `try_osr` (`vm/src/runtime/interpreter/jit_bridge.rs`) | snapshots locals and tags, admits (§2), enters, drains the pending-exception / NPE / AIOOBE / arithmetic flags, routes the `i64::MIN` deopt sentinel |
| Exit maps | `deopt_points` tagged `DeoptReason::OsrExit`, `osr_exit_points`, `can_osr_exit` (`jit/src/lib.rs`); `jit/src/osr_exit.rs` classifies where an exit landed | loop-boundary snapshots for a mid-loop bail |
| In-place exit transfer | `transfer_osr_exit_into_live_frame` / `transfer_osr_exit_into_live_frame_checked` (`vm/src/runtime/interpreter/deopt_resume.rs`) | overwrites the LIVE frame's locals/stack/pc from a reconstructed frame — the same-frame counterpart of `resume_real_ir_deopt`. A frame with an inlined caller chain goes to `transfer_osr_exit_chain_into_live_frame` instead. |

So: HIR *is* built for a loop bci, compiled code *is* entered, and a deopt
*can* return to the loop state. Three things were missing.

**(a) No type check, at all.** `osr_enter` takes `jit_locals: &[i64]` — raw
words with no types attached. Nothing compared the interpreter's idea of a slot
against the compiled entry's. A `double` local seeded into a GPR home, or a
`long` seeded where the compiled body reads a reference, is a silent
miscompile. The `Frame::get_local_tag` accessor that exists *precisely* "for
JIT/OSR interop" (`vm/src/runtime/frame.rs:1473`) was never passed to the JIT.

**(b) No operand-stack story.** The trampoline seeds locals only. A back-edge
entry has an empty operand stack by construction, so this has never bitten —
but nothing said so, and nothing would notice if a future trigger fired at a
bci where it is not true.

**(c) The refusal that costs a replay.** Every existing refusal is an
`Option::None`: the reason is discarded, and — worse — several of them are
decided *after* compiled code has already run. That is the recorded
`jit-osr-bail-reruns-loop-iterations` defect: 20 000 requested iterations,
20 008 executed; 12 346 requested, 42 730 executed when the exception escaped
the OSR'd method.

---

## 2. What this change adds

A **typed admission check** in `jit/src/lib.rs`, built on metadata the artifact
already carries. No new `CompiledMethod` field, no producer change, no
behaviour change to the existing `osr_enter` path.

| Item | Where |
| --- | --- |
| `OsrSlotType` — the six-point slot lattice (`int/long/float/double/ref/top`), with `from_vtag` (interpreter side) and `from_frame_value` (compiled side) | `jit/src/lib.rs` |
| `OsrSlotExpectation` — `Exact(t)` from a precise `FrameState`, or `Integral` / `FloatingPoint` inferred from the register-home maps, or `Unconstrained` / `NotSeeded` | `jit/src/lib.rs` |
| `OsrEntryState` — the interpreter's offer: pc, locals + tags, stack + tags | `jit/src/lib.rs` |
| `CompiledMethod::validate_osr_entry` — the whole admission check | `jit/src/lib.rs` |
| `OsrEntryPlan` — the proof of admission, carrying the exact `resume_bci` | `jit/src/lib.rs` |
| `OsrEntryPlan::resume_after_exit` — the *only* sanctioned post-entry resume point for a flat (non-inlined) exit | `jit/src/lib.rs` |
| `CompiledMethod::osr_enter_planned` — a thin wrapper spending the plan on the existing `osr_enter` (it calls `osr_enter(vm_ptr, state.locals, plan.entry_pc, thread_ptr)`) | `jit/src/lib.rs` |
| `osr_refusal_is_permanent` — which refusals may be memoed via `mark_osr_entry_rejected` | `jit/src/lib.rs` |
| `CompiledMethod::osr_home_disagreement` — cross-checks the precise `FrameState` against the register homes the trampoline seeds through (added after this change; see §3) | `jit/src/lib.rs` |

The VM side of this is wired: `try_osr` builds an `OsrEntryState` from
`Frame::get_local_raw` / `Frame::get_local_tag`, calls `validate_osr_entry`, and
enters through `osr_enter_planned`. See `docs/jit/osr-vm-side-wiring.md`.

### Where the per-slot expectation comes from

Two contract strengths, because two artifact shapes exist:

* **Precise** (`OsrContractSource::PreciseFrameState`). The artifact carries a
  `FrameState` at the entry bci — the `OsrExit`-tagged deopt point, i.e. the
  loop-boundary snapshot, falling back to any deopt point at that bci. Every
  slot's exact JVM type is known. Only populated when `CRATONVM_DEOPT_REAL` was
  on at compile time.
  *Using the exit map as the entry contract is the point*: entry and exit
  describe the same program point's live state, which is exactly the symmetry
  "survive forced deopt" is asking for.

* **Inferred** (`OsrContractSource::RegisterHomes`). The production case: no
  deopt metadata. The typing left is the register-home map —
  `osr_xmm_assignments[i].is_some()` means the compiled body reads that slot as
  FP; `osr_local_assignments[i].is_some()` means it reads it as an integral or
  reference word. Coarse, but it catches precisely the mismatch class that
  silently corrupts a frame.

Under the inferred contract an incoming `top` (uninitialized) is **accepted**:
the dead mask only names the *hazardous* dead locals (those sharing a register
with a live one), so a harmlessly-dead local arrives unmasked and legitimately
uninitialized. Refusing those is what an earlier blanket dead-mask
refusal did, and it cost H2's hottest method every one of its OSR entries.
Under the precise contract an incoming `top` against a live `Exact` type **is**
refused: the contract knows liveness, so the disagreement is real.

---

## 3. Refusal taxonomy

Every refusal is a `bailout::Bailout` carrying
`BailoutReason::UnsupportedShape(tag)` and a context string naming the method,
bci and offending slot. Each is counted through `bailout::record_bailout`, so
they land in the existing `unsupported_shape` category and are visible to the
compiler report. **No panic, and no bare `None` with the reason thrown away.**

| Tag | Raised when | Permanent? |
| --- | --- | --- |
| `osr-entry-no-table` | `osr_pc_to_native` is `None` — never compiled for trampoline entry | yes |
| `osr-entry-pc-not-an-entry` | no native offset at this bci (past the end, or the `-1` inside a hoisted body) | yes |
| `osr-entry-dead-local-mask` | non-zero dead mask under `CRATONVM_JIT_OSR_DEAD_LOCALS=0` | yes |
| `osr-entry-local-count` | locals/tags slices disagree, or the offer does not match `osr_num_locals` | no |
| `osr-entry-operand-stack` | live operands: the trampoline seeds locals only | no |
| `osr-entry-slot-type-mismatch` | an incoming slot's JVM type is not one the entry accepts there | no |
| `osr-entry-undescribable-slot` | the entry contract's own slot is `Unsupported`, `MaterializationRequired`, or scalar-replaced | yes |
| `osr-entry-returnaddress-slot` | an incoming slot holds a `jsr` return address | no |
| `osr-entry-inlined-scope` | an inlined region the deopt metadata cannot describe — see below | yes |
| `osr-entry-unconditional-trap` | `has_indy_trap`: the body bails on every execution reaching that site | yes |
| `osr-entry-unresumable-exit` | some *other* deopt point of this artifact reconstructs an unresumable frame, or the precise entry contract holds monitors | yes |
| `osr-entry-ambiguous-exit-image` | `osr_exit_policy`: `osr_exit::first_ambiguous_resume_bci` finds two deopt points at one bci whose `ResumeSemantics` disagree, so the resume bci itself would be arbitrary. Points that differ only in `reason` are **not** refused; `has_reason_ambiguous_bci` counts them (`osr_entry_reason_ambiguous_image`) | yes |
| `osr-entry-contract-disagreement` | the artifact's two views of its own entry frame disagree: the precise `FrameState` at the entry bci says one register file for a local, the register homes the trampoline seeds through say another (`osr_home_disagreement`, masked-dead slots skipped). Checked before the per-slot loop, so a compiler bug is not reported as an interpreter slot mismatch | yes |
| `osr-exit-replay-refused` | post-entry: the reconstructed frame cannot name an exact resume point | n/a |

"Permanent" means the answer is a pure function of the artifact, so the caller
may memo it through `mark_osr_entry_rejected` instead of re-running the
pipeline on the next back-edge. A state-dependent refusal must **not** be
memoed: the next trip carries different locals. The two sets are the constants
`OSR_REFUSAL_TAGS` (14 tags) and `OSR_PERMANENT_REFUSAL_TAGS` (9), and
`osr_refusal_is_permanent` is the predicate; the tag strings are an external
contract (tests and log greps key on them) and must not be renamed with the
code that raises them. Every refusal is built by the single constructor
`osr_refusal`, which is what counts it.

### The two entry doors

`try_osr` (`vm/src/runtime/interpreter/jit_bridge.rs`) can enter compiled code
through either of two doors, and only one of them goes through the taxonomy
above:

| Door | Artifact table | Entered by | Admitted by |
| --- | --- | --- | --- |
| Single-pass | `CompiledMethod::osr_pc_to_native` | `osr_enter_planned` → `osr_enter` → `osr_trampoline` | `validate_osr_entry` (this document) |
| Optimizing (IR tier) | `CompiledMethod::ir_osr_entries`, emitted by `ir_lower`'s `emit_osr_entry_stubs` | `CompiledMethod::ir_osr_enter` | the lowerer's own construction, plus `ir_osr_enter`'s length check |

When `ir_osr_entry_addr(entry_pc)` is `Some`, `try_osr` skips
`validate_osr_entry` entirely. That check proves the offered slots against the
single-pass entry contract (`osr_num_locals`, the per-method local assignments),
and an SSA body has none of those. An optimizing-tier stub has the ABI
`extern "C" fn(ctx: i64, locals: *const i64) -> i64` and builds its own frame.
The lowerer emits a stub for a bci only when every value live on entry is named
by that bci's safepoint snapshot, or is a shape the stub can produce itself
(`osr_entry_can_produce`: `Op::Const` and `Op::ConstF`). `ir_osr_enter` returns
`None` when the caller offers fewer locals than the stub reads. The stubs are
gated by `CRATONVM_JIT_IR_OSR_ENTRY`, default **on** since 2026-09-05 (`=0` is
the kill switch); the optimizing door in `try_osr` additionally requires the
artifact's `ir_osr_sentinel_free`.

> The item doc comments on `CompiledMethod::ir_osr_entries` and
> `ir_osr_entry_addr` still say the table is empty on every artifact and that
> nothing calls the stubs. That contradicts `ir_osr_entry_enabled` (default on)
> and the `ir_osr_enter` call in `try_osr`. This document follows the code.

### The machine transition: `osr_trampoline`

x86-64 only (`#[cfg(target_arch = "x86_64")]`). `osr_enter` resolves
`target_addr = entry + osr_pc_to_native[entry_pc]` and the per-pc `dead_mask`,
refuses (`None`) a non-zero mask when `CRATONVM_JIT_OSR_DEAD_LOCALS=0`, and
calls `osr_trampoline` with the artifact's recorded frame layout.

**The cache.** `osr_trampoline` looks the body up in `osr_trampoline_cache()`, a
process-global map keyed by **`(target_addr, dead_mask)`**. The key is not
`target_addr` alone because the body bakes in the per-pc dead-local skip set,
and two OSR pcs share a native target whenever the instruction between them
emits no bytes. With `target_addr` alone, a hit for the second pc reused the
first pc's skip set, either skipping a live local or seeding a dead one over a
coalesced live register. On a miss the body is emitted outside the lock and
inserted under it; if another thread raced, its entry wins. A code pointer
that `validate_code_ptr` rejects makes the call return `None`, and the caller
keeps interpreting. When a `CompiledMethod` is dropped, its drop path removes
every cache entry whose `target_addr` falls inside the artifact's buffer. (The
first paragraph of `osr_trampoline_cache`'s own doc comment predates the key
change and still says it is keyed by `target_addr` and never evicted.)

**The emitted body** (`emit_osr_trampoline`). It takes `(locals_ptr, vm_ptr,
thread_ptr)` in the platform C ABI (RCX/RDX/R8 on Windows, RDI/RSI/RDX on
System V). It must mirror every step of `x64::Compiler::emit_prologue`
(`jit/src/x64/frames.rs`) that the body entered past it relies on. In emission
order:

1. `push rbp; mov rbp, rsp`.
2. **Stack bang, before `sub rsp`.** When `x64::jit_stack_bang_enabled()`
   (default on; `CRATONVM_JIT_NO_STACK_BANG` or `CRATONVM_JIT_STACK_BANG=0`
   turns it off), one `MOV EAX, [RSP + disp32]` per displacement from
   `stack_bang_frame_probe_disps(frame_size)`: one probe per 4096-byte page the
   frame crosses, plus the exact frame bottom when it is not page-aligned. An
   OSR entry near the end of the stack otherwise skipped the guard page and
   faulted instead of raising `StackOverflowError`. A frame that would need
   more than `MAX_STACK_BANG_PROBES` (512) probes gets no trampoline (`None`).
3. `sub rsp, frame_size`, then, with the bang enabled, the one-page headroom
   probe at `[RSP - 4096]`. This is the prologue's
   `emit_stack_bang_before_frame_alloc` / `emit_sub_rsp_imm` /
   `emit_stack_bang_headroom` sequence.
4. **Safepoint-id sentinel.** When `sp_id_slot_off != 0` and
   `sp_id_slot_init_enabled()` (default on; `CRATONVM_JIT_ZERO_SPID=0` turns it
   off), store `x64::safepoint::SP_ID_UNSET_BC_PC` (`u32::MAX - 1`) into
   `[rbp - sp_id_slot_off]`. Until the body's first safepoint the slot otherwise
   held whatever the previous frame at that depth left, and a stack walk could
   match a real oop map on it. The prologue writes the same sentinel, but later
   (after the parameters are homed) and additionally gated on `precise_maps`.
5. Stash `locals_ptr` in R10.
6. Spill the callee-saved GPRs to exactly the slots the epilogue restores them
   from: `osr_callee_saved_regs` (the compiler's `alloc_used_regs`), in that
   order, at `[rbp - (callee_saved_base + i*8)]`. Artifacts without that
   metadata fall back to deriving the set from `local_assignments` in
   `LOCAL_REGS` order. Then spill `osr_callee_saved_xmms` as full 128-bit
   `MOVUPS` at `x64::xmm_save_slot_offset`, since Win64 preserves XMM6–XMM15.
7. Store `vm_ptr` into the heap-local slot when `needs_context`, and
   `thread_ptr` into the JIT-thread slot. Store `-1` into the native-stack-floor
   slot, so every self-call site in an OSR-entered frame takes the out-of-line
   stack guard.
8. **TLS frame record.** When `frame_record != 0` and the inline TLS
   displacement is available, `MOV <gs|fs>:[disp32], RBP` and then, when
   `compile_id != 0`, `MOV dword <gs|fs>:[cm_disp], compile_id`, the identity
   half of the pair the prologue writes. Before this, only RBP was published, so
   the id read 0 until the body's first compiled call returned. The collector
   then fell back to a stack decode, which fails for a frame whose return
   address is a trampoline. When the inline form is unavailable (or the verify
   knob is on), the trampoline instead calls the `frame_record` helper, saving
   R10 and `thread_ptr` in `[rsp+32]` / `[rsp+40]` across the call.
9. Shadow stack: cache `thread_ptr` in the shadow-thread slot and, when it is
   non-null, save the shadow `top` watermark into the savetop slot. With a
   partial layout, only zero the thread slot.
10. For each local `i`: skip it if bit `i` of `dead_mask` is set (`i < 64`).
    Otherwise load `[R10 + 8*i]`, write the frame slot only when the local has
    no register home (or `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS=1`), then move the
    value into its GPR home and/or `MOVQ` it into its XMM home.
11. `MOV RAX, target_addr; JMP RAX`. A buffer that overflowed its size estimate
    is discarded (`None`).

### The inlined-scope refusal

> **Status, 2026-09-12.** The two premises this subsection was written on have
> both moved. `FrameState::caller` is no longer `None` at every producer: the
> single-pass backend's deopt-point publisher (`jit/src/x64/deopt_stubs.rs`)
> sets `frame_state.caller = self.inline_caller_chain()`, which is non-`None`
> inside a spliced body (since 2026-08-18). The IR lowerer's own producers still
> write `caller: None`. And the VM transfer is no longer single-frame:
> `transfer_osr_exit_into_live_frame_checked` hands a frame with caller frames
> to `transfer_osr_exit_chain_into_live_frame`, which writes the outermost scope
> into the live frame and pushes the rest.
>
> What the code raises `osr-entry-inlined-scope` for today:
>
> * `validate_osr_entry`, when the artifact inlined something, has deopt
>   points, and **no** deopt point records a caller chain (the condition
>   below, unchanged);
> * `validate_osr_entry`, when the precise entry contract itself has a caller
>   scope;
> * `osr_exit_policy`, when a deopt point's `deopt::caller_chain_depth` exceeds
>   `deopt::MAX_OSR_INLINE_RESUME_DEPTH` (9). A chain within that budget is
>   admitted, not refused;
> * `OsrEntryPlan::resume_after_exit`, for any exit carrying caller frames.
>   That function is the flat-exit resume; the chain path does not call it.
>
> The last paragraph of this subsection, and the matching `// A chain that IS
> recorded … has no multi-frame path` comment in `validate_osr_entry`, are
> therefore stale. They are kept below for the reasoning.

`FrameState::caller` was hard-coded `None` at every producer when this was
written, and the inliner does inline. Two consequences, only one of which is an
OSR problem:

* **Entry is safe.** `osr_pc_to_native` is indexed by the *outer* method's code
  array, so an entry pc is always an outer-scope block start. An OSR entry
  cannot land inside an inlined region.
* **Exit is not.** A frame-deopt taken inside an inlined callee is reported at
  the *callee's* bci under the *outer* method's key, indistinguishable from an
  outer-scope exit. Resuming it lands the interpreter at a bci that means
  something else entirely.

So `validate_osr_entry` refuses when the artifact inlined something, can take a
frame-deopt exit, and **no** deopt point records a caller chain. It is written
that way — rather than "inlined and has deopt points" — so it relaxes on its own
as producers start populating `FrameState::caller`; see
`docs/jit/deopt-inline-scopes.md`, which closes the IR-side representation.
In production `deopt_points` is empty, so inlined methods keep their OSR entries
and are governed by `OsrExitPolicy::PropagateOnly` instead.

A chain that *is* recorded is refused for a narrower, separate reason: the VM's
in-place transfer is single-frame (`transfer_osr_exit_into_live_frame` bails on
`"inlined caller chain"`), so a multi-frame exit still cannot be resumed. That
refusal should be lifted the same day the transfer grows a multi-frame path.

---

## 4. How no iteration is re-run

The defect this must not reproduce: compiled code commits N iterations, bails,
and the interpreter resumes at the *entry* bci from its own stale locals —
replaying all N.

The guarantee is structural, not a check:

1. **The entry bci is the interpreter's own pc.** `OsrEntryPlan` is derived from
   `OsrEntryState::pc`; there is no separate `entry_pc` argument that could
   disagree with it. At a taken back-edge the interpreter has already advanced
   `frame.pc` to the loop header and executed zero bytes of the new iteration
   (`vm/src/runtime/interpreter.rs:9771` — `let entry_pc = frame.pc;`). So
   `plan.resume_bci == plan.entry_pc` repeats nothing *by construction*.

2. **A refusal happens before entry, and has no side effects.**
   `validate_osr_entry` reads metadata and the caller's slices, allocates one
   `Vec`, and never enters compiled code. Falling back to `state.pc` after a
   refusal is therefore always correct.

3. **After entry, `resume_bci` is not a resume point.** The only sanctioned
   post-entry resume is `resume_after_exit`, which returns the *reconstructed
   frame's own* bci — never the entry bci unless the bail genuinely landed back
   on the loop header having completed a whole number of iterations, which is
   what a loop-boundary `OsrExit` map means. When the frame cannot name where
   it is, it returns `Err`: the caller must propagate, and has no sanctioned way
   to fall back to the entry bci.

4. **An exit that could not be described is refused at admission, not at exit.**
   `osr_exit_policy` walks every deopt point of the artifact before the entry is
   admitted and refuses (`osr-entry-unresumable-exit`) if any of them
   reconstructs an unresumable frame. Discovering that *after* entering is
   useless: by then the only options are to replay the committed iterations or
   to lose them. This is the "make the re-entry point exact, or refuse" rule.

5. **Only a `REEXECUTE` bci is a place the interpreter may be parked.**
   `DeoptimizationPoint::semantics` (`jit/src/deopt.rs:701`, landed alongside
   this change) distinguishes three meanings of a snapshot bci, and the
   distinction is the same defect one bytecode down:

   * `REEXECUTE` — the bytecode at `bci` has not taken effect; `frame.pc = bci`
     runs it, which is exactly right. An `OsrExit` at a loop header is this
     (`ResumeSemantics::for_reason(OsrExit) == REEXECUTE`): the header iteration
     has not run.
   * `RESUME` — the bytecode already took effect and the interpreter must
     continue *after* it. Parking at that bci re-executes it. The successor bci
     needs the method's bytecode, which `jit` does not have, so such a point
     refuses the entry at admission (`osr-entry-unresumable-exit`) and refuses
     again on the way out.
   * `RETHROW` — not a resume point at all. Such points may legitimately exist
     in an artifact (they are stashed separately, via `take_exceptional_frame`,
     and never routed to a resume), so they do not disqualify the entry — but a
     frame that arrives naming one is refused.

`OsrExitPolicy` records which of the two worlds an admitted entry is in:

* `ExactTransfer` — every frame-deopt exit reconstructs a resumable frame in
  this method's own scope; a bail transfers the JIT-advanced loop state into the
  live frame and resumes at the bail's bci.
* `PropagateOnly` — the artifact has no frame-deopt exits at all (the production
  default, since `deopt_points` is empty unless `CRATONVM_DEOPT_REAL` was on).
  Control leaves the body by returning or through the exception routes, which
  propagate out of the frame. The interpreter must never "resume" this frame at
  the entry bci.

---

> **Status of §5 and §6, 2026-09-12: both prescribed edits have landed.**
> The transfer is now `transfer_osr_exit_into_live_frame_checked` in
> `vm/src/runtime/interpreter/deopt_resume.rs`. It refuses a
> `FrameValue::MaterializationRequired` slot under its own name, and refuses
> `VirtualObject` / `VirtualObjectRef` slots and held monitors, all before the
> first write to the live frame. It consults `OsrEntryPlan::resume_after_exit`
> for the resume bci. `try_osr` (`vm/src/runtime/interpreter/jit_bridge.rs`)
> builds the `OsrEntryState` from `get_local_raw` / `get_local_tag`, admits
> through `validate_osr_entry`, memoes permanent refusals, and enters through
> `osr_enter_planned`. See `docs/jit/osr-vm-side-wiring.md`. The file paths and
> line numbers quoted in §5 and §6 are those of the time of writing, and the
> code shapes below are the prescriptions, not the current source.

## 5. Finding: `MaterializationRequired` in the in-place OSR transfer

**Out of scope for this change (`vm/src/runtime/interpreter.rs` is not ours),
reported here.**

`transfer_osr_exit_into_live_frame` special-cases exactly one marker
(`vm/src/runtime/interpreter.rs:14045`):

```rust
if matches!(v, cratonvm_jit::deopt::FrameValue::Unsupported) {
    locals.push(None);                 // leave the live frame's slot alone
} else {
    match fv_to_value(v) {
        Some(val) => locals.push(Some(val)),
        None => return bail("unmappable local"),
    }
}
```

`FrameValue::MaterializationRequired` is **not** special-cased, so it falls into
`fv_to_value`, whose catch-all arm returns `None`
(`vm/src/runtime/interpreter.rs:13320`), and the transfer bails.

**Verdict: it does not fall through wrongly — it refuses, which is the correct
direction.** But it refuses at the worst possible moment, and the refusal path
is the known-bad one:

* The `bail("unmappable local")` propagates to `try_osr`'s `can_osr_exit &&
  transfer(...)` check (`vm/src/runtime/interpreter/invoke.rs:15679`), which
  falls through to the **safe reject** — "continue interpreting THIS frame from
  where it was".
* That is correct only when the bail precedes any committed iteration. Here it
  does not: the OSR'd body ran, so every iteration since entry executes a second
  time. The comment block at `:15450` documents exactly this failure mode for
  the sibling exception path.

Two edits are wanted, neither in this change's scope:

1. **`vm/src/runtime/interpreter.rs:13959-13972`** — the Phase-A scope guard
   rejects `VirtualObject | VirtualObjectRef` but not `MaterializationRequired`.
   Add it there, with its own `bail("materialization required")` label, so the
   diagnostic names the real cause instead of the generic "unmappable local"
   (that is the whole reason the variant was split out of `Unsupported`).
2. **`vm/src/runtime/interpreter/invoke.rs:15679`** — a rejected transfer after
   a *committed* OSR body must not take the safe-reject path. It should
   propagate (as the exception routes now do via `propagate_osr_exception`) or,
   better, never arise: gate the entry on
   `CompiledMethod::validate_osr_entry`, which refuses up front
   (`osr-entry-unresumable-exit`) any artifact whose deopt points include a
   `MaterializationRequired` slot.

Note that `deopt::frame_state_is_resumable` already treats
`MaterializationRequired` as non-resumable (`jit/src/deopt.rs:398`), so item 2
falls out for free once the entry gate is wired.

---

## 6. Interpreter-side call shape

`try_osr` (`vm/src/runtime/interpreter/invoke.rs:15296`) currently builds a
bare `Vec<i64>` and calls `osr_enter`. The validated entry needs the tags
alongside the words. `Frame::get_local_tag` already exists for exactly this
(`vm/src/runtime/frame.rs:1473`, doc: *"Get the legacy VTAG byte of a local (for
JIT/OSR interop)"*), so the VM-side change is additive and small:

```rust
// vm/src/runtime/interpreter/invoke.rs, replacing the jit_locals loop at :15340
let frame = &thread.frames[frame_idx];
let num_locals = frame.locals_len();
let mut jit_locals = Vec::with_capacity(num_locals);
let mut jit_local_tags = Vec::with_capacity(num_locals);
for i in 0..num_locals {
    jit_locals.push(frame.get_local_raw(i) as i64);
    jit_local_tags.push(frame.get_local_tag(i));
}
// The operand stack at a taken back-edge is empty; pass it explicitly so a
// future non-back-edge trigger is refused rather than silently truncated.
let osr_state = cratonvm_jit::OsrEntryState {
    pc: entry_pc,                       // == frame.pc at every call site today
    locals: &jit_locals,
    local_tags: &jit_local_tags,
    stack: &[],
    stack_tags: &[],
};

let plan = match compiled.validate_osr_entry(&osr_state) {
    Ok(plan) => plan,
    Err(b) => {
        if cratonvm_jit::osr_refusal_is_permanent(&b) {
            crate::jit::mark_osr_entry_rejected(
                &class_name, &method_name, &method_descriptor, entry_pc,
            );
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!("[cratonvm-jitc] OSR-refuse {b}");
        }
        // Nothing ran: continue interpreting THIS frame at `plan`-less
        // `entry_pc`, which is where it already is.
        return None;
    }
};
// … unchanged guard/thread setup …
unsafe { compiled.osr_enter_planned(vm_ptr, &osr_state, &plan, thread_ptr) }
```

and, on the exit side, replacing the bare `rframe.bci` the transfer trusts
(`vm/src/runtime/interpreter.rs:14076`, `frame.pc = rframe.bci as usize;`):

```rust
let resume_bci = match plan.resume_after_exit(&compiled, &rframe) {
    Ok(bci) => bci,
    Err(b) => {
        // MUST NOT safe-reject: the body committed iterations. Propagate.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!("[cratonvm-deopt] OSR-exit unresumable: {b}");
        }
        return None; // after routing the frame out, not resuming the loop
    }
};
```

Two properties the VM side must preserve:

* `OsrEntryState::pc` must be the frame's **current** pc. Every call site today
  passes `entry_pc = frame.pc` after the branch was taken
  (`vm/src/runtime/interpreter.rs:9771`, `:9817`, and the sibling `if_*`
  arms), so this holds — but it is the invariant, not an accident, and a new
  trigger must respect it.
* `get_local_tag` must be read from the **same** frame snapshot as
  `get_local_raw`, before `set_jit_thread`, so no intervening safepoint can
  retype a slot between the word and its tag.

`can_osr_enter` is unchanged and stays the cheap pre-filter that decides whether
to run the pipeline at all; `validate_osr_entry` is the admission check on the
finished artifact and subsumes it (`osr-entry-no-table`,
`osr-entry-pc-not-an-entry`, `osr-entry-dead-local-mask` are the same three
answers, now with reasons).

---

## 7. To reconcile

* ~~**`FrameState::caller` is still `None` at every producer.**~~ **Partly
  lifted:** the single-pass backend's deopt-point publisher
  (`jit/src/x64/deopt_stubs.rs`) records `inline_caller_chain()` inside a
  spliced body; the IR lowerer's producers still write `caller: None`.
  `osr-entry-inlined-scope` still refuses an inlined artifact with deopt points
  none of which records a chain.
* ~~**The VM's OSR-exit transfer is single-frame.**~~ **Lifted:**
  `transfer_osr_exit_chain_into_live_frame` (`vm/src/runtime/interpreter/deopt_resume.rs`)
  transfers a chain, and `osr_exit_policy` admits one up to
  `deopt::MAX_OSR_INLINE_RESUME_DEPTH` (9). See the status note in §3.
* **The precise contract only exists under `CRATONVM_DEOPT_REAL`.** Production
  artifacts get the register-home inference, which cannot distinguish `int` from
  `long` from `ref` within a GPR home. Closing that means emitting a per-entry-pc
  slot-type table from the codegen — a `jit/src/x64.rs` / `jit/src/ir_lower.rs`
  edit, out of scope here. The classifier it would come from
  (`classify_local_kinds`, `jit/src/x64/bce.rs`) is `pub(super)` and not
  reachable from `lib.rs` today.
* **Operand-stack seeding.** `validate_osr_entry` type-checks the stack and then
  refuses any non-empty one, because `osr_trampoline` has no stack-seeding path.
  If a future trigger wants to OSR at a bci with live operands, the trampoline
  needs the seeding code before the refusal can be lifted.
* **`osr_enter_planned` is `#[cfg(target_arch = "x86_64")]`**, mirroring
  `osr_enter`. The validator itself is architecture-independent, so an aarch64
  OSR path can reuse it unchanged.
