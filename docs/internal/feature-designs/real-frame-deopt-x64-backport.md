# Real-Frame-Deopt — x64 Backend Backport (Phase A→production)

Status: **Phase A IMPLEMENTED — all 6 steps landed, behind the default-off
`CRATONVM_DEOPT_REAL` gate (gate-off byte-identical to production).** Effort: **L**
(the gating slice of the XL keystone; Phases B/C deferred). Companion to
[`real-frame-deopt.md`](real-frame-deopt.md), which landed the mechanism on the
dormant IR path. This doc scoped bringing it to the **production single-pass x64
backend** so it actually deopts real workloads.

> Scoped via a multi-agent understand→design→adversarial-review pass over the
> seven x64 subsystems involved. `file:line` citations below are from that pass
> and reflect the tree at the time of writing — verify before editing, code moves.

## Current status (2026-06-22)

**Phase A is functionally complete.** Steps 1–6 are all implemented and behind the
default-off `CRATONVM_DEOPT_REAL` gate; with the gate off the codegen is
byte-identical to production (bt18=68332206).

- **Steps 1–5** landed on `dev` as part of the deopt-osr scaffolding workstream
  (snapshot at the BCE pilot guard → 3-arg `x64_deopt_entry` + in-stub 16-GPR
  spill → build+validate the interpreter `Frame` → flip the resume at both sinks →
  `can_deopt_resume` coverage gate + `CRATONVM_DEOPT_VERIFY` verifier). The cat-2/FP
  resume extensions (`SavedRegisters.xmm[16]`, `RegisterLong`/`RegisterRef`/
  `XmmFloat`/`XmmDouble`), the epoch-invalidation de-speculation, and the true
  OSR-exit state transfer all landed too — see [`real-frame-deopt.md`](real-frame-deopt.md)
  and the deopt-osr feature doc for the full step-by-step record.
- **Step 6** (widen to call-site canonical-boundary guards) landed on branch
  `feat/x64-deopt-backport` (worktree `CratonVM-x64deopt`), commit `a7bff464`,
  **not yet merged to dev**:
  - new `snapshot_pre_intrinsic_call(bci, reason)` — idempotent call-site snapshot
    taken at the canonical flush boundary (post-`flush_scratch_registers`, pre-pop),
    inserted into `deopt_box_ptr_by_bci`;
  - `emit_deopt_stubs` routing extended so reason 6 (`ReceiverTypeChanged`,
    String-intrinsic guards) goes through the frame-deopt trampoline when a
    snapshot is present, same as reason 2 (`BoundsCheck`);
  - 7 snapshot sites: arraycopy, `String.charAt/length/isEmpty/hashCode`,
    `String.equals`, `String.compareTo`, `String.indexOf(char)`,
    `String.indexOf(String)`, `CRC32.update`;
  - unique `cratonvm-x64deopt` binary in `vm-cli/Cargo.toml` for worktree isolation.
  - Validation: 835 JIT + 81 deopt-JIT + 48 deopt-VM tests pass; gate-off
    byte-identical.

**What is NOT done (deliberately out of Phase-A scope):**
- **Production default-on flip.** The feature is proven under the gate but stays
  default-off. Flipping it on is coupled to precise OSR-frame tracking
  (`CRATONVM_SHADOW_OSR_TRACK`), which currently regresses bt18 under moving GC
  (conservative-pin × precise-move) — co-scheduled with the precise-JIT-maps /
  moving-GC workstream, not this doc.
- **Phases B/C**: `VirtualObject` materialization for scalar-replaced objects on
  the x64 path, inlined-frame chains, and monitor-bearing methods remain excluded
  by the positive `can_deopt_resume` gate.

The decisions captured in "Open question" below were resolved as the steps landed
(BCE loop-header pilot, primitive-width source via typed local kinds,
`SavedRegisters.xmm[16]` added for the cat-2/FP follow-up, compilation-epoch
versioning wired). They are retained for historical context.

## Phase B — x64 VirtualObject emission (scalar-replaced objects)

Scoped from a fresh research pass (2026-06-22) over the x64 scalar-replacement
path, the VM-side materializer, and the IR-path producer. Branch
`feat/x64-deopt-phase-b` (worktree `CratonVM-x64deopt`).

**Consumer is already done.** `vm/src/runtime/deopt_materialize.rs`
(`materialize_virtual_objects`) is fully implemented, wired into the live resume
sink (`build_deopt_frame_inner`), GC-correct (two-phase, `TempRootScope`),
cycle-safe, and unit-tested. The IR/C2 producer already feeds it via
`ir_lower::frame_value_for_object` under `CRATONVM_SCALAR_DEOPT`. So **Phase B is
producer-only work in `jit/src/x64.rs`** — emit the right `VirtualObject`
descriptors; no consumer changes.

**The gap.** x64 scalar replacement (`plan_scalar_replacement`,
`ScalarReplacedObject`) keeps only `{num_fields, field_base_offset}` — it drops
the class id and all per-field typing, and the snapshot builder
(`build_and_record_deopt_point`) emits a bogus `StackSlotRef`/`RegisterRef` to the
zeroed dummy ref instead of a `VirtualObject`. `can_deopt_resume` requires
`scalar_replaced.is_empty()`, excluding every scalar-replacing method.

**Two load-bearing simplifications vs the IR path** (both reduce risk):
1. **Frame slots are temporally correct by construction.** The IR path lowers SSA
   *values* and so needs strict-dominance analysis (a deopt before a field store
   must see the pre-store value). The single-pass path reads the field's *frame
   slot*, which is zero-filled at `new` and overwritten by `putfield` — so the
   slot always holds the field's actual value at any PC. No dominance analysis;
   reading the slot is unconditionally correct.
2. **Scalar objects appear only in LOCALS at deopt points, never on the operand
   stack.** `analyze_escapes` escapes any object passed as a call arg, so a
   non-escaping (scalar-replaced) object is never a call argument → never on the
   stack at a Step-6 call-site deopt; and a BCE loop-header deopt has an empty
   stack. So only **per-local** provenance is needed (`local index → new_pc`),
   which is index-keyed (no operand-stack-depth alignment risk). The straight-line
   guarantee (`analyze_escapes` escapes anything crossing a CFG edge) makes
   clearing provenance at branch barriers sound — no scalar object is ever live
   across one.

**Field types** come from access sites: join `field_info` (`(pc, field_index,
type_tag)`) with the plan's `field_ops` (`pc → new_pc`) on pc to get
`(new_pc, field_index) → type_tag`. Fields never accessed default to `Int(0)`
(zero, sound — admitted SR allocations have no non-zero primitive `<init>`),
mirroring the IR producer's `None ⇒ Int(0)`.

**Increments:**
- **B1 — data model.** `ScalarReplacedObject` gains `class_id` (already in
  `new_info`, currently dropped); Compiler gains `sr_field_types:
  HashMap<(new_pc, field_index), u8>` (built post-plan from `field_info ∩
  field_ops`) and `sr_local_prov_at: HashMap<pc, Vec<(local_idx, new_pc)>>`
  (captured during `plan_scalar_replacement`'s existing locals abs-interp,
  snapshotting non-empty `local_prov` per pc). Additive, behavior-neutral.
- **B2 — emit.** In `build_and_record_deopt_point`, for each local `i` whose
  `sr_local_prov_at[bci]` maps it to a scalar `new_pc`: emit
  `FrameValue::VirtualObject` (first occurrence) / `VirtualObjectRef(new_pc)`
  (shared), with `field_values[k] = StackSlot{,Ref,Long}(-(field_base_offset +
  k*SLOT_SIZE))` typed by `sr_field_types`, or `Int(0)` for unaccessed fields.
- **B3 — relax the gate.** `can_deopt_resume` admits a scalar-replacing method
  when it has **no monitor ops / is not `ACC_SYNCHRONIZED`** (the elided-monitor
  hazard stays in Phase C) and every live SR object fully resolves (no
  `Unsupported`/`Undefined` field). Still `CRATONVM_DEOPT_REAL`-gated.
- **B4 — tests + e2e** under `CRATONVM_DEOPT_REAL` / `CRATONVM_DEOPT_EAGER`.

**Status (2026-06-22): B1–B3 implemented, unit + integration tested, committed.**
849 jit tests (2 new: `plan_scalar_replacement` class_id + `local_prov_at`
capture; `sr_field_values` type→`StackSlot*` mapping) + 48 vm deopt tests
(consumer materialization) all green. The field-slot offset
(`-(field_base_offset + k*SLOT_SIZE)`) was verified by code to match exactly the
address the SR `getfield` reads (`emit_load_local`/`modrm_rbp_disp`). Gate-off is
byte-identical (emission only runs inside `build_and_record_deopt_point`, gated;
`can_deopt_resume` stays false with no `deopt_points`).

**B4 runtime e2e — BLOCKED on single-pass SR reachability (not a Phase B defect).**
Added a `CRATONVM_DEOPT_EAGER_BCI=<n>` trigger (a call-site analog of
`CRATONVM_DEOPT_EAGER`, since a scalar object can never be live at a loop header)
to force a deopt at a straight-line bci where a scalar local is live. Diagnostics
(`CRATONVM_DBG_SCALAR_DEOPT`) showed single-pass scalar replacement never fires at
runtime for the standard `new X(); <init>()V` pattern: `jit_scan` runs
`analyze_escapes` with an EMPTY invokespecial-shape map, so it conservatively
escapes the `<init>` receiver and seeds `non_escaping_new` empty; `x64::compile`'s
precise re-analysis (which has the resolved shapes) is gated to only *refine a
non-empty* set (`if non_escaping_new.is_empty() { stays empty }`), so it never
runs — and the hot IR/`try_compile` path passes an empty set too (`lib.rs:5812`).
Net: single-pass (x64) SR was effectively dormant in the runtime hot paths for
ordinary allocations.

**SR-reachability FIXED (commit `0db926ed`).** `x64::compile`'s precise
re-analysis is now gated on whether the method allocates (`new_info` non-empty)
rather than on jit_scan's seed, so it DISCOVERS non-escaping objects (it
recomputes from scratch with the resolved `<init>` shapes) instead of only
refining a non-empty set. Verified: a straight-line `new X(); <init>()V` method
now reports `non_escaping=[0]` and the Phase B path emits the `VirtualObject`
(correct `class_id`/`num_fields`) at runtime. This is an UNGATED production
codegen change — validated by 849 jit tests + binarytrees depth-16 checksum
`14985902` == HotSpot (escaping `TreeNode`s correctly NOT scalar-replaced).
Fuller app-suite validation (gauntlet/kafka) recommended before merging the
ungated change.

**Live deopt-RESUME still not directly observed — C2 subsumption.** The eager
trigger fires in the single-pass (C1) compile, but C2/IR subsumes hot methods
(and has its own IR SR-deopt via `ir_lower::frame_value_for_object`,
`CRATONVM_SCALAR_DEOPT`), so the *running* version of a hot method is usually the
C2 recompile — single-pass deopt-resume is inherently narrow at runtime (C1
window / C1-resident methods only). Every link is proven independently (SR fires
→ producer emits the correct `VirtualObject`; the consumer materializes it, 48 vm
tests), but the C1-version-stays-resident-and-deopts case is hard to stage. The
trigger + diagnostics are retained as the harness.

## Phase C — monitors (IMPLEMENTED); inlining (out of scope)

**DONE — branch `feat/x64-deopt-phase-c`, commit `2c814346`.** A method holding a
`synchronized(obj)` block over a scalar-replaced object now deopt-resumes: the
elided monitor is recorded at the deopt point and re-acquired on the
re-materialized object on resume.

- **Producer** (`jit/src/x64.rs`): `plan_scalar_replacement` gained explicit
  monitorenter/monitorexit handlers tracking per-scalar-object recursion depth +
  a per-PC held-monitor snapshot (`monitor_at`) and the scalar-receiver monitor PC
  set (`monitor_scalar_ops`). `build_and_record_deopt_point` emits
  `MonitorInfo{ VirtualObjectRef(new_pc), lock_depth }`. The codegen monitor
  handler sets `has_elided_monitor` only for elisions over NON-scalar objects, so
  `can_deopt_resume` admits scalar-monitor methods. Handling monitors explicitly
  (vs the catch-all clear-all) also **fixed a latent miscompile**: a field access
  inside `synchronized(scalarObj)` previously lost provenance and was compiled as
  a heap access on the elided dummy ref.
- **Consumer**: `deopt_materialize` rewrites each held monitor's `VirtualObjectRef`
  to the materialized shell; `build_deopt_frame_inner` pins + re-reads the monitor
  objects through the same GC-forwarding flow as locals/stack, then re-acquires
  each via `shared.monitors.enter` `lock_depth` times after the frame is built.
  `ACC_SYNCHRONIZED` methods still bail (the method monitor over `this` is a
  separate case).
- Gated `CRATONVM_DEOPT_REAL`; gate-off byte-identical for monitor-free methods.
  Tests: 850 jit (+1) + 50 vm deopt (+2, incl. a resume that relocks at depth 2
  and proves ownership). A `synchronized`-block-over-scalar program == HotSpot
  (`20000100000`) gate-off AND gate-on; bt16 = `14985902`.

The same C2-subsumption caveat as Phase B applies to the live monitor-relock
path: every link is unit/integration-proven, but a hot method's running version
is usually the C2 recompile, so single-pass monitor deopt is narrow at runtime.

**Inlining is default-OFF with a known unfixed `try_emit_inline_body` miscompile**,
so inlined-frame-chain deopt is moot in production and remains excluded.

## Goal

When a guard fails in JIT code, rebuild a **precise** interpreter frame at the
trapping bci — locals / operand stack reconstructed from live machine state —
**in the deopt stub, before the epilogue** — instead of returning the `i64::MIN`
sentinel and re-running the whole method from bci 0
(`vm/src/runtime/interpreter.rs:17043` → CacheMiss; slow sink `:17322` →
`Ok(None)`). This kills the side-effect double-execution that blocks all
speculative JIT optimization.

**Phase-A scope (this doc):** single non-inlined methods; GPR + frame-slot
provenance only; **no** `VirtualObject` materialization (`deopt.rs:735` is still
a panic stub); **no** inlined-frame chains; **no** FP/XMM-live, category-2-live,
or monitor-bearing methods — all excluded by a positive `can_deopt_resume` gate.

## Relationship to the IR-path Phase A (what is reused vs new)

**Reused verbatim** (the backend-agnostic *consumer* + *resolver*): `FrameValue`
/ `FrameState` / `DeoptimizationPoint`; `reconstruct_frame_from_machine_state`
(`deopt.rs:844`), `SavedRegisters` (`deopt.rs:764`, `repr(C)`, `gpr[16]`),
`resolve_value` (`deopt.rs:788`: `Register(r)→gpr[r]`, `StackSlot(off)→*(rbp+off)`),
`take_last_deopt` (`deopt.rs:870`); the `CompiledMethod.deopt_points` /
`_deopt_point_boxes` / `find_deopt_point` plumbing (`lib.rs:831,838,1148`). The
**in-stub trampoline ordering** (call the deopt entry with `rbp` *live* BEFORE
`add rsp / pop rbp / ret`, `ir_lower.rs:851-860`) and the **per-guard
boxed-pointer keying** (`ir_lower.rs:650-669`) are reused as the structural model.

**Superseded** (the IR-specific SSA producers — cannot run single-pass):
`ir_lower.rs` `frame_value_for` (`:786`, NodeId→FrameValue, StackSlot/Int only),
`resolve_frame_state` (`:876`), `build_deopt_points` (`:891`), `emit_deopt_stub`
(`:845`); `ir.rs` `SafepointSnapshot` recording. x64 reimplements their
equivalents over its own state.

**Net-new beyond the IR equivalents** (these were the load-bearing review
findings — the IR path deferred all three):
1. **Populated `SavedRegisters` + Register resolution.** `ir_deopt_entry` passes
   `SavedRegisters::default()` (`deopt.rs:893`) and the IR lowerer spills every
   value, so `resolve_value`'s `Register` arm (`deopt.rs:790`) has **never run**.
   x64 is the first producer to emit `FrameValue::Register` — requires a **3-arg**
   entry carrying the spilled register file (the 2-arg `ir_deopt_entry` can't).
2. **A real type source.** The IR path tags everything `Int`. x64 must emit
   `Object` from a *positive* oop source and primitives from a *new* width source
   (see "Genuinely new", below).
3. **VM-side interpreter resume.** The IR path only stashes `LAST_DEOPT`
   (`deopt.rs:895`); pushing real interpreter `Frame`s is entirely new, modeled on
   `route_jit_exception_through_method` (`interpreter.rs:6697`).

## Reuse map (hosted by x64 today)

| Existing | Location | Reused as |
|---|---|---|
| Guard+stub framework: group by `(bci,reason)`, shared stub, forward-JMP patch | `x64.rs:12459-12534` | **Extended** with a new frame-deopt stub branch; existing `jit_uncommon_trap` stubs untouched |
| Whole-GPR blind-spill at safepoints (SB-CRASH-04 `safepoint_reg_spill` / `reg_spill_base`) | `x64.rs:6310-6331`, `emit_pre_safepoint_spill:6300` | Same `emit_store_local` spill pattern, **extended to all 16 GPRs** laid out for `SavedRegisters.gpr` (RAX=0..R15=15) |
| Per-slot location model: `StackSlot` enum {Frame/CalleeSaved/Scratch/Xmm}, `local_assignments`/`xmm_assignments` from regalloc, `reg_for_local`/`xmm_for_local`/`local_offset` | `x64.rs:5134-5153,5210,5219,7017-7024,6047` | The **provenance source**: `reg_for_local(i)→Register`, else `StackSlot(-local_offset(i))`; operand `Frame(off)→StackSlot(-off)`, `CalleeSaved/Scratch(r)→Register(r)` |
| Coverage gate `fully_oop_covered` (`safepoint_pcs ⊆ mapped ∧ !osr ∧ inline_sites empty`) | `x64.rs:21206-21210` | Exact shape mirrored for `can_deopt_resume` |
| `local_oop_masks` / `local_oop_reached` / `stack_oop_marks` | `x64.rs:5485,5488,5471,2298-2351` | **Positive oop tag ONLY** — `Object` iff bit SET *and* reached. **Never** infer `Int` from a clear bit (see Risks) |
| Interpreter deopt sinks (`result==i64::MIN && deopt_signaled`) | `interpreter.rs:17043,17322`; `take_jit_deopt_pending:16966` | The branch points. They **cannot** read machine state (frame already torn down) — they only TAKE a pre-built `LAST_DEOPT` frame |
| `route_jit_exception_through_method` (Frame::new_pooled, refill_pools, push_frame_and_fire_entry → FramePushed) | `interpreter.rs:6697,6808-6843` | Structural **model** for VM-side resume, incl. the GC-root-before-refill ordering |

## Genuinely new components (`build_new`)

1. **3-arg x64 deopt entry** in `jit_integration.rs`:
   `x64_deopt_entry(point: *const DeoptimizationPoint, rbp: u64, regs: *const SavedRegisters) -> i64`
   — reads the spilled GPR file, calls `reconstruct_frame_from_machine_state`,
   stashes `LAST_DEOPT`, returns `i64::MIN`. (`ir_deopt_entry` has no register
   param.)
2. **`emit_deopt_snapshot_at_guard(reason)`** — emits a `DeoptimizationPoint` AT
   the eligible guard (NOT at call-return safepoints; the BCE pilot at
   `x64.rs:13182` is a loop header, not a safepoint, so `emit_oop_map_for_safepoint`
   never runs there). Walks locals `0..num_locals` and `self.stack[i]`, tags
   `Object` only from the positive oop source, primitives from the new width
   source, `Undefined` otherwise. Boxes the point; bakes the box ptr as imm64 into
   the guard's deopt branch (mirror `ir_lower.rs:650-669`).
3. **Primitive type/width source** — the single most under-scoped item.
   `local_oop_masks`/`stack_oop_marks` are oop-vs-not **binary** and cannot
   distinguish int/long/float/double or cat-1 vs cat-2. Thread the verifier
   `StackMapTable` type state (or a small primitive-width shadow) so each non-oop
   live slot gets a concrete `Int`/`Float`/width. **Treat as a new subsystem, not
   a sibling of `emit_oop_map_for_safepoint`.**
4. **Frame-deopt stub variant** inside `emit_deopt_stubs`: spill 16 GPRs into the
   `SavedRegisters` layout, `arg0`=boxed point ptr (imm64), `arg1`=rbp,
   `arg2`=lea spill region, `CALL x64_deopt_entry` **before** `emit_epilogue`, set
   `JIT_DEOPT_PENDING`, RAX=`i64::MIN`.
5. **`Compiler.deopt_points` + `deopt_boxes` fields + finalize transfer** (mirror
   `cm.oop_maps = compiler.oop_maps` at `x64.rs:21189`). This producer is new —
   x64 has **zero** references to `deopt_points` today (grep-verified).
6. **`can_deopt_resume` per-method flag** (mirror `fully_oop_covered`):
   `precise_maps ∧ !compiled_via_osr ∧ inline_sites.is_empty() ∧
   scalar_replaced.is_empty()` (excludes monitor-elision *and* VirtualObject)
   `∧ no XMM-live / cat-2-live slot at any frame-deopt guard ∧ every guard
   FrameState fully typed (no Undefined live slot) ∧ !ACC_SYNCHRONIZED`. When
   false the method stays entirely on the `i64::MIN` re-run path.
7. **Canonical-boundary eligibility filter** — frame-deopt only at block
   boundaries where Scratch/Xmm operand caches are flushed
   (`x64.rs:5146-5152`); mid-bytecode guards (e.g. div-by-zero `x64.rs:12113-12126`,
   operands already popped → short stack) are excluded in Phase A.
8. **VM-side resume materializer** — extends the `interpreter.rs:17043`/`:17322`
   sinks: when `CRATONVM_DEOPT_REAL` on AND `cm.can_deopt_resume` AND
   `take_last_deopt()==Some` for this invocation → root the frame oops, refill
   pools, build one `Frame` via `Frame::new_pooled` (cat-2 two-slot expansion,
   pc=bci), `push_frame_and_fire_entry`, return `FramePushed`. Root oops **before**
   `refill_pools_from_shared` (GC hazard).
9. **`CRATONVM_DEOPT_REAL`** env gate (default off) so new-resume and `i64::MIN`
   re-run are never both live for a method; consulted identically at **both** sinks.
10. **`CRATONVM_DEOPT_VERIFY`** test-only eager-deopt differential verifier:
    deopt at every eligible guard, reconstruct, compare reconstructed-interpreter
    result vs JIT result — catches map/regalloc drift (`real-frame-deopt.md:267-270`).
    CI-mandatory before any guard family is flipped.

## Design: the in-stub reconstruction model

The single biggest correction over the naive plan: **the `i64::MIN` interpreter
sink runs AFTER the JIT method returned — `rbp` and the GPRs are gone.** So all
machine-state capture + reconstruction happens **inside the deopt stub before the
epilogue** (exactly as the IR path does, `ir_lower.rs:855` call before `:857-860`
epilogue). The sink only *takes* the pre-built `LAST_DEOPT` frame and pushes
interpreter `Frame`s.

**Keying:** each guard bakes a pointer to its own boxed `DeoptimizationPoint` as
an imm64 the stub loads — **not** `find_deopt_point`-by-native-offset. This
sidesteps the native-offset-anchoring trap entirely (the BCE guard at `13205-13211`
is emitted before `pc_to_native[pc]` at `13217`). `find_deopt_point` is reserved
for any future call-return-safepoint deopt.

**Resume disambiguation:** resume keys on `take_last_deopt()` returning `Some` for
*this* invocation, **NOT** on the `i64::MIN` value — which collides with a legit
`Long.MIN_VALUE` return (documented `interpreter.rs:17086-17091`). Clear
`LAST_DEOPT` before every JIT call; both sinks consult the identical predicate.

## Implementation steps (ordered, each independently landable)

1. **Snapshot at the BCE pilot guard, emit-and-discard.** Add `Compiler.deopt_points`
   + `deopt_boxes`; `emit_deopt_snapshot_at_guard()` called only at the speculative-BCE
   loop-header guard (`x64.rs:13187`). Build `FrameState` from regalloc; tag `Object`
   positively, primitives from the new width source, `Undefined` otherwise. Box +
   bake imm64 into a not-yet-routed slot. No control-flow change, nothing reads it.
   *Lands:* unit test asserts a point exists at THAT guard's bci with locals matching
   regalloc (Register for a register-allocated IV, StackSlot for a spilled local,
   Object for the array ref). *Gate:* none (additive). *Risk:* low — except the new
   primitive/width source is the real work; emit `Undefined` + `can_deopt_resume=false`
   rather than guess.
2. **3-arg entry + in-stub GPR spill + reconstruct (stash, no resume).** Add
   `x64_deopt_entry`; extend `emit_deopt_stubs` with the frame-deopt branch; route
   ONLY the pilot guard here behind `CRATONVM_DEOPT_REAL`. *Lands:* unit test — pilot
   guard fails at runtime, `take_last_deopt()` returns a frame whose Register-resolved
   locals equal the LIVE register values (non-zero — proves the spill+3-arg wiring;
   the IR default-zeros path would fail this). *Gate:* new reason; only pilot routes;
   stashed not resumed → no behavior change without the flag. *Risk:* med — `gpr`
   indexing must be RAX=0..R15=15; call must precede epilogue.
3. **Build + validate the interpreter Frame (no resume yet).** At the sink, when
   flag+gate+`Some`: root oops, refill pools, build a `Frame` from the resolved
   locals/stack — then DISCARD it and still return CacheMiss. Assert the built frame's
   locals/stack/pc equal the interpreter's own re-run frame for a side-effect-free,
   cat-2-free pilot. *Lands:* end-to-end (flag on) equality + a `CRATONVM_GC_STRESS`
   variant with a young-gen oop local proving the temporary-rooting. *Gate:* built +
   discarded → still CacheMiss. *Risk:* med — cat-2 + GC-rooting exercised here,
   de-risked because nothing resumes.
4. **Flip the resume (both sinks).** Replace discard+CacheMiss with
   `push_frame_and_fire_entry` + `FramePushed`; apply the IDENTICAL branch at the
   slow sink (`:17322`, vs `Ok(None)`). Clear `LAST_DEOPT` + `take_jit_deopt_pending`
   once. *Lands:* a method that deopts after a side-effecting bytecode resumes at the
   guard bci WITHOUT re-running the side effect (the exact double-exec bug). *Gate:*
   `CRATONVM_DEOPT_REAL`; off → both sinks behave as today. *Risk:* **high** — first
   Frame pushed from machine state; mitigated by Step 3 + `can_deopt_resume` exclusions.
5. **Coverage gate finalize + eager-deopt verifier.** Finalize `can_deopt_resume`;
   add `CRATONVM_DEOPT_VERIFY`. *Lands:* verifier clean across jit + VM smoke; a
   deliberately one-slot-shifted snapshot is caught; each exclusion (XMM/cat-2/monitor/
   inlined/OSR/under-typed) gets a negative test. *Gate:* verifier test-only;
   `can_deopt_resume` is the per-method kill-switch.
6. **Widen to other canonical-boundary non-speculative guards + measure.** Route
   additional canonical-boundary guards one family at a time, still gated; wire
   `DeoptimizationLog` (`deopt.rs:159`) for deopt-rate/most-common-reason. *Lands:* a
   representative kafka/h2 method that today re-runs on a bounds/null guard shows
   precise resume; no fast-path throughput regression.

## Risks (load-bearing, grounded)

- **Sink runs post-return** — all capture is in-stub before the epilogue; any design
  reading `SavedRegisters`/`rbp` at the sink is incorrect.
- **`i64::MIN` value collision** — resume must key on `take_last_deopt()==Some`, not
  the value+flag; both sinks identical; clear `LAST_DEOPT` before each JIT call.
- **Clear oop bit is NOT primitive** — `compute_local_oop_masks` is an intersection
  dataflow (`x64.rs:2332`); a clear bit conflates primitive, dead, and
  conditionally-oop. Tagging clear→`Int` mistypes a live oop as a primitive (GC-root
  mistake, no conservative backstop on the resume path). Only SET+reached → `Object`;
  primitives need the separate positive source; else `Undefined` → `can_deopt_resume=false`.
- **Elided monitors dropped on resume** — `monitorenter`/`exit` emit nothing under
  scalar replacement (`x64.rs:20376-20396`); a resumed frame's `monitorexit` hits an
  un-entered monitor. Phase-A gate: `can_deopt_resume=false` if `scalar_replaced`
  non-empty OR `ACC_SYNCHRONIZED` OR any monitor op present.
- **Mid-bytecode guards capture a short/non-canonical stack** — div-by-zero pops
  operands before the test; Scratch/Xmm caches flush before calls/branches. Phase A
  restricts to canonical block boundaries.
- **XMM/FP and cat-2 have no resolver slot** — `SavedRegisters` is `gpr[16]` only.
  `can_deopt_resume=false` for any XMM-resident or cat-2 live slot at a guard, until
  `SavedRegisters` gains `xmm[16]` + a width source (follow-up).
- **GC during resume** — `refill_pools_from_shared` (`:6808`) can GC before the Frame
  is pushed; root the reconstructed oops first (mirror
  `route_jit_exception_through_method`). GC-stress test required.
- **Recompilation invalidation** — boxed imm64 ptrs + a FrameState encode a specific
  regalloc; tie `can_deopt_resume` to a compilation epoch, fall back to `i64::MIN` on
  MakeNotEntrant until versioned points land; assert each point's region ⊆ owning
  `CompiledMethod` code range.
- **Map drift** — a one-slot disagreement silently restores garbage; mitigated by the
  mandatory eager-deopt verifier and the canonical-boundary restriction (snapshot and
  codegen read the same state at the same `cur_bc_pc`).

## Open question — pilot guard choice (a real judgment call)

The sequencing reviewer flagged that the design doc says "route one **non-speculative**
guard first" (`real-frame-deopt.md:247-250`), but this plan pilots the **speculative**
BCE loop-header guard. The synthesis chose BCE deliberately: it sits at a **canonical
block boundary** (clean operand stack, int IV + array ref, all GPR/canonical), whereas
the non-speculative div-by-zero guard is **mid-bytecode** (operands already popped →
short stack, harder to snapshot). The argument: for piloting the *mechanism*,
canonical-boundary cleanliness matters more than speculative-vs-not, and resume is
proven (Steps 1–4) before any real speculation is *flipped* — so speculation
correctness stays isolated from the deopt machinery. If a non-speculative guard at a
clean boundary exists, it would satisfy both constraints; otherwise this is the
recommended tradeoff. **Decide before Step 1.**

Other open questions: primitive/width source granularity (full StackMapTable vs a
minimal primitive shadow — StackMapTable is a Step-1 prerequisite for any non-int-only
method); whether to add `SavedRegisters.xmm[16]` now vs Phase B; explicit
compilation-epoch versioning of deopt points; and the runtime de-speculation action
when a guard deopts repeatedly (fall back to `i64::MIN` re-run vs MakeNotEntrant).
