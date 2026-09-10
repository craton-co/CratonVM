# Wiring linear scan into `ir_lower`

> **Status banner, 2026-09-10.** This file describes the FIRST increment, when
> the path was XMM-only and default **off**. Both halves of that are now stale:
> `CRATONVM_JIT_IR_LINEAR_SCAN` is **default ON**, there is a **GP** file as
> well as an FP one (`regalloc::xmm_roles::IR_GP_LINEAR_SCAN`), and
> `ir-drop-home` / `ir-deopt-regs` / `ir-reg-authoritative` have since made some
> home stores droppable. Read `docs/config/flag-inventory.md` for what is
> actually on.
>
> What is NOT stale is this file's central claim, and it is the reason to keep
> reading it: **write-through buys loads, not stores.** That ceiling was
> re-measured from the other side on 2026-09-10 by widening the GP file to the
> seven registers Win64 offers — residency rose, splits halved, and the code
> got **4.6% slower**. See
> `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.

`jit/src/regalloc.rs` has had a complete, self-verifying linear-scan register
allocator (`allocate_linear_scan`, `verify_allocation`, `resolve_parallel_copy`)
and no production consumer — which is what
the C2 review item 3 is about, and what
`docs/jit/linear-scan-regalloc.md` documents from the allocator's side.

This document is the *consumer* side: what `ir_lower.rs` now does with that
allocator, how to turn it on, why it is safe at a safepoint, and — at least as
important — what it still does not do.

---

## The one-paragraph version

Behind `CRATONVM_JIT_IR_LINEAR_SCAN` (default **off**), `lower_inner_with_scopes`
runs the linear-scan allocator over a four-register XMM file, verifies the
result, cross-checks it against `plan_slots`' independently computed live
ranges, and uses what survives as a **register read cache**: a value the
allocation keeps in one register for its whole life is copied into that register
at its definition, and its later reads become register moves instead of frame
loads. Every value is still written to its frame slot exactly as before. The
frame image is unchanged; only reads get cheaper.

---

## What is wired

| Piece | Where |
|---|---|
| Flag | `ir_lower::linear_scan_enabled` (`CRATONVM_JIT_IR_LINEAR_SCAN`) |
| Register file | `ir_lower::IR_LOWER_LS_XMMS` = XMM2–XMM7 |
| Machine model | `ir_lower::ir_lower_machine_model` — `MachineModel::for_graph` plus this backend's own clobbers |
| The call site | `ir_lower::plan_register_residency` → `regalloc::allocate_linear_scan` + `regalloc::verify_allocation` |
| Install | `Lowerer::set_residency`, once, before the prologue |
| Read | `Lowerer::fp_load_value` |
| Write | `Lowerer::fp_store_value`, `Lowerer::publish_fp_from_slot` |
| Metrics | `metrics::note_current_spills` / `note_current_reloads`, on this path only |

`RUST_LOG`-free diagnostics: `CRATONVM_DBG_IR_LINEAR_SCAN=1` prints one line per
compile with the position count, the peak live set, how many deopt pins were
released, what the scan promoted, what survived the cross-checks, and what was
demoted.

### Enabling it

```
CRATONVM_JIT_IR_LINEAR_SCAN=1
```

**The flag is not declared yet.** `types/src/flag_groups.rs` owns the declared
inventory and is outside this change. Until an entry is added there,
`flags::runtime_var` falls through to a live `std::env` read: the environment
variable works, but `-XX:` options and `flags::with_thread_overrides` do not
reach it. The unit tests deliberately do not depend on either — they drive a
`#[cfg(test)]` thread-local override (`LsForce`) — so declaring the flag later
cannot silently turn them vacuous.

---

## The value model this had to fit into

Before deciding anything, the shape of `ir_lower` as it actually is:

* every value has a **frame slot** (`node_slot`), coloured by `plan_slots` so
  values with disjoint live ranges share a word;
* every operand read is a load from that word and every result write is a store
  to it. Counted before this change: **36 `self.load_to_rax`, 16
  `self.load_to_rcx`, 38 `self.store_rax`** — 90 GP memory accesses — plus the
  FP tier's `fp_load` / `fp_store`, all fed by **83 `self.slot_of` call sites**.
  (The brief for this work guessed "roughly 80 sites through
  `load_to_rax`/`store_rax`". 80 is close to the `slot_of` count, not the
  load/store count.) After this change `self.slot_of` is down to 64: the 19 FP
  operand reads now name the *value* rather than its frame offset, which is
  the shape the rest of the conversion would take.
* the lowerer tracks **no** register residency at all. RAX/RCX are a per-node
  scratch tier, XMM0/XMM1 their FP analogue, and nothing survives a node
  boundary in a register.

Converting all 90 sites in one wave is the "stop treating the frame as the
value's identity" change, and it is not an increment — it is the whole item.
What follows is the increment.

---

## Design: a write-through read cache

A promoted value gets a register **in addition to** its frame slot, never
instead of it:

```
        colourer path                    with the cache
  ────────────────────────────    ────────────────────────────
  MOVSD xmm0, [rbp-a]             MOVAPS xmm0, xmm2        ; a is resident
  MOVSD xmm1, [rbp-b]             MOVAPS xmm1, xmm3        ; b is resident
  MULSD xmm0, xmm1                MULSD  xmm0, xmm1
  MOVSD [rbp-r], xmm0             MOVSD  [rbp-r], xmm0     ; unchanged
                                  MOVAPS xmm4, xmm0        ; publish r
```

Three consequences, and they are the reason for the shape:

1. **The frame image is complete at every instruction boundary.** Nothing that
   reads a frame word had to change: `emit_safepoint_map`, `build_deopt_points`,
   `emit_phi_copies`, call-argument marshalling, the shadow-stack push. None of
   those files or functions were touched.
2. **A definition site nobody converted costs an optimization, not
   correctness.** Residency is published by the *definition*, through
   `Lowerer::reg_live`; a read consults `resident_xmm`, which checks
   `reg_live`. An unconverted definition arm therefore leaves the value
   memory-only for its whole life. There is no edit that becomes wrong code by
   omission. A source-level test (`the_register_read_path_is_gated_on_publication`)
   pins that invariant: every read of the assignment goes through one of two
   accessors.
3. **It buys loads, not stores.** The store side is still one per definition.
   That is the honest ceiling of this increment.

---

## The register file, and why it is this small

`regalloc::RegFile::x86_64()` offers `LOCAL_REGS` (RBX, R12–R15, plus RSI/RDI on
Win64), R8/R9 and XMM8–XMM15. **None** of them was usable here at one point,
for a reason that had nothing to do with the allocator:

> `ir_lower::emit_prologue` saves **no callee-saved register**. It pushes RBP,
> subtracts the frame, stores the ABI arguments to their local slots, and that
> is all.

That is no longer true of the XMM half. `ir_lower::IR_LOWER_SAVED_XMMS` is a
callee-saved XMM save area — XMM6/XMM7 on Windows, empty on System V where
every XMM is volatile — emitted in the prologue and restored at all three exits
(`emit_epilogue`, `emit_call_exc_stub`, and `emit_deopt_stub`'s inlined
teardown). It sits at `spill_cap_off`, inside the band
`conservative_roots::band_slot_is_verifiable` already skips, so the reader side
needed no edit. The GP half of the sentence still stands: writing R12 would
still corrupt the Rust caller.

What is available is the set that is **untouched by this emitter and either
caller-saved or saved by this frame**:

| Register | Used by `ir_lower` for |
|---|---|
| RAX, RCX | the GP value tier (every opcode) |
| RDX | `IDIV`, `FCmp`'s `SETcc` |
| R10, R11 | thread pointer, shadow-stack cursor, safepoint flag address |
| R8, R9 | call-argument marshalling; SysV entry ABI args 5–6 |
| XMM0, XMM1 | the FP value tier |
| **XMM2–XMM7** | **nothing** |

So: XMM2–XMM7. The ceiling is XMM7 and it is an **encoder** limit, not a frame
one — `fp_load` / `fp_store` / `fp_binop` emit ModRM as `(xmm & 7) << 3` with no
REX.R, so XMM8+ is not addressable by them at all. Raising it is a REX change in
those three functions.

XMM6/XMM7 come out `caller_saved: false` on Windows, which is the actual return
on the save area: a value may now live in one **across a call** there. On System
V they are `true` like the rest, because the ABI makes them volatile and no
prologue can change that. The back-edge safepoint poll was narrowed to match —
it clobbers the caller-saved subset rather than the whole file, the same rule
`MachineModel::for_graph` applies at a call, sound because the poll's slow path
is an ordinary `extern "C"` function.

**This keeps the wiring FP-only.** An `int` loop counter still gets nothing, and
that is now a *safepoint* question rather than a prologue one: a GP file can
hold references, so it has to discharge the oop-map obligation per site instead
of structurally. See `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
"Where the safepoint / oop-map obligation lives".

It also makes the GC question moot by construction, which is worth having on top
of the argument below: `RegClass::of(IrType::Ref) == Gp`, and this file offers no
GP register, so no reference can be register-resident at all.

---

## Safepoints

The rule is unchanged from `docs/jit/linear-scan-regalloc.md`: **references stay
in memory across every safepoint.** Here it holds three times over.

1. **Structurally.** The file is XMM-only; references are `RegClass::Gp`. A
   `Ref` cannot be promoted because there is no register for it. Tested by
   `a_reference_is_never_register_resident`.
2. **By the allocator.** `allocate_linear_scan` refuses to promote a `Ref` whose
   range covers a safepoint, and `verify_allocation` re-checks it independently
   (proof 5).
3. **By write-through.** Even a promoted value's home word holds it at every
   instruction boundary, so `emit_safepoint_map`'s frame-slot publication is
   exactly as complete as it was before the allocator existed.

### The map consumer, checked

`vm/src/jit/conservative_roots.rs` (read-only for this change) consumes
`OopMapEntry::frame_slot_offsets` — *frame slot offsets*, `i16` displacements
from RBP — plus `frame_layout`, `live_frame_hi` and `sp_id_slot_off`. **There is
no register bank in the map, and no code path that would rewrite a register on
an evacuation.** Publishing a reference in a register would therefore need work
in the map format, the frame walker and the deopt frame reconstructor before it
could even be attempted. Nothing here goes near that: the map is byte-identical
to what the colourer-only path produces.

### Calls the op model does not see

Two clobber gaps were found and closed while doing this. Both would have
destroyed a caller-saved register the allocator thought was live:

* **`regalloc::ir_op_is_call` was not a superset of `ir_lower`'s calls.** It
  named `Call`, `New`, `NewArray` and `LambdaIntToDouble`. `ir_lower` also emits
  a `CALL` that returns into the body for `Op::Rem` on `Float`/`Double`
  (`jit_frem`/`jit_drem`), `Op::Load(_)` (`jit_getfield`) and `Op::Store(_)`
  (`jit_putfield_int`). The predicate now names all seven, with a test.
  `Op::Guard` is deliberately still absent: its failure edge runs the epilogue
  and never returns into the body, and counting it would deny a register to
  every value in a bounds-checked loop for no gain.
* **The cooperative safepoint poll belongs to no node.** On a loop back edge
  `lower_terminator` emits `TEST byte [flag] ; JZ ; CALL slow_path` between the
  terminator and the edge — a position `MachineModel::for_graph` marks a
  *safepoint* but not a *clobber*. `ir_lower_machine_model` adds it, tested by
  `the_machine_model_clobbers_the_back_edge_poll`.

---

## The deopt pins, and why nothing would promote without releasing them

This is the finding that decides whether any of this does anything on real code.

`build_live_model` pins every value a `SafepointSnapshot` names, because "the
home must hold the value at *any* recorded bci" is not a property a register
allocator establishes. `ir::IrBuilder` records a snapshot of the locals **and
the operand stack** at *every bytecode boundary*. Every temporary is on the
stack across at least one boundary. So on any graph built from bytecode the pin
set is essentially the whole value set, and `allocate_linear_scan` promotes
**nothing at all** — it is not conservative there, it is inert. Measured, not
inferred: `a_bytecode_graph_pins_everything_until_the_deopt_pins_are_released`
asserts that most values are pinned before the release and that the scan
promotes strictly more after it.

`LiveModel::release_deopt_pins(graph)` drops the deopt pins and keeps the phi
pins. It is sound for **this** consumer for exactly one reason: write-through.
The home store is emitted at every definition whether or not the value also got
a register, so a deopt frame read out of home words is bit-identical to the one
the colourer-only path builds. `build_deopt_points` is untouched and needs no
change.

One trap worth naming, because it very nearly made the whole thing inert a
second time: `plan_slots` keeps its own pins, so every deopt-named value is
`SlotClass::Pinned` — "never shares a frame word". Refusing to promote that
class in the consumer would have re-pinned exactly the values the release
frees. It does not; it refuses only phis, whose home is written by
`emit_phi_copies` and not by any definition arm that could publish a register.
`a_deopt_named_value_is_still_promotable` is the witness.

The price, stated in that function's contract: a caller that releases pins
**must not** use `Allocation::stack_slot` / `stack_slots`, because releasing a
pin also moves the value out of the `Pinned` home pool and the allocation's own
home colouring may then share a word between two deopt-named values. `ir_lower`
takes every home offset from `plan_slots` (which keeps all pins) via
`alloc_slot_checked`, and reads `Allocation::stack_slot` nowhere. Nothing
enforces that mechanically; it is why releasing pins is an explicit call and not
the default.

---

## Fail-closed: what refuses, and what merely declines

| Situation | Outcome |
|---|---|
| `verify_allocation` rejects the allocation | **`Err` → `refuse()`** — the method loses its optimized body and runs in a lower tier. The only hard stop, because it is the only compiler-bug signal |
| `allocate_linear_scan` bails (register pressure, unsatisfiable fixed constraints, split budget) | promote nothing — a property of the input program, not a reason to lose the body |
| Liveness fixed point did not converge | promote nothing |
| `build_live_model`'s position count disagrees with the schedule | promote nothing |
| `build_live_model`'s `wants_loc` disagrees with `plan_slots`' coloured set | promote nothing |
| A value's allocation has more than one segment (a split / spill / reload) | that value is not promoted |
| A value is not `Float`/`Double`, has no home colour, or is a phi | that value is not promoted |
| Two values share a register but `plan_slots` says their ranges overlap | **both** lose the register, counted as `demoted` |
| A clobber falls inside a value's `plan_slots` range | that value loses the register, counted as `demoted` |

Declining is always safe: a value with no register keeps the frame slot the
lowerer has always given it, and every read of it is the load it always was. A
non-zero `demoted` count is a compiler bug worth chasing — two liveness models
disagreeing — but it is not wrong code.

---

## What `verify_allocation` does and does not prove

It proves seven things about the allocation (shape, register class, no
aliasing, ABI/clobbers, no `Ref` in a register across a safepoint, home-word
disjointness, event bookkeeping) — see `docs/jit/linear-scan-regalloc.md` for
the list.

It proves them **against the `LiveModel` and `MachineModel` it is handed**. It
cannot know:

* whether that liveness model matches the one the emitter's slot layout came
  from. That is why `plan_register_residency` re-runs the aliasing and clobber
  questions against `plan_slots`' own ranges, and demotes on disagreement.
* whether the `MachineModel`'s clobber list matches what the backend actually
  emits. A missing clobber is invisible to it — which is exactly how the two
  gaps above (`ir_op_is_call`, the back-edge poll) would have got through. Those
  are now tested directly rather than trusted.
* anything about the **emission**. It says a value may hold XMM3 over
  `[12, 40]`; it does not say the emitter put it there, kept it there, or read
  it from there. The publication interlock (`reg_live`) and the end-to-end
  execution test (`the_register_cache_changes_no_result_and_drops_no_home_store`)
  are what cover that.

---

## Metrics

`CompilationReport::spills` / `::reloads` are now fed — on this path only.

They report **what the backend emitted**, not what `Allocation` planned. The
allocation is a full spill/reload/split schedule; this wiring executes only the
subset it can prove, so publishing `Allocation::spills` would describe
instructions nobody generated. Concretely:

* `spills` — one per resident definition, the write-through home store;
* `reloads` — the memory→register materialisations, i.e. values whose defining
  arm computes into RAX (`Op::ConstF`, FP `Op::Neg`, FP `Op::Param`) and reach
  their register through the home word.

With the flag off — the default, and every compile today — both stay
`NotMeasured`. That is the truth, not a gap: the lowerer allocates no registers,
so there is no register↔memory transition to count, and a reported `0` would be
indistinguishable from "an allocator ran and spilled nothing". Keeping those two
distinguishable is the entire reason `Measured<T>` exists.

---

## What is **not** done

Listed so nobody reads a green test suite as a finished item.

1. **Not measured on a benchmark.** No CratonBench run, no before/after on an FP
   kernel. The claim here is "correct and enabled", not "faster". The report's
   15–45% estimate is for a *general* allocator; this one covers four XMM
   registers and no store elimination, and should be expected to deliver a small
   fraction of it.
2. **Integer values get nothing.** The register file is FP-only. The blocker is
   **no longer the prologue**: `IR_LOWER_SAVED_XMMS` has landed, with
   the restore on all three exits (`emit_epilogue`, the inlined epilogue in
   `emit_deopt_stub`, and `emit_call_exc_stub`) and the area placed at
   `callee_saved_lo` so the conservative band scan does not read a caller's
   register as a possible root. Extending it to RBX/R12–R15 is mechanical.

   What is left is the reason that was always underneath: a GP register can hold
   a **reference**, and `OopMapEntry` describes frame slots only. The file being
   XMM-only is what makes "no reference is register-resident at a safepoint"
   true structurally; a GP file has to prove it per site instead, by spilling to
   the home word before every safepoint the way `x64.rs` does
   (`SafepointPublishPlan::no_reference_in_registers`). That is the increment,
   and it is a bigger one than the save area was.
3. **Stores are not eliminated.** Write-through is what makes the safepoint,
   deopt and phi arguments hold without touching those paths. Dropping the home
   store means teaching `emit_safepoint_map`, `build_deopt_points` and
   `emit_phi_copies` to read a register — i.e. a register bank in the oop map
   and in the deopt frame reconstructor, which `vm/src/jit/conservative_roots.rs`
   does not have.
4. **Splits and reloads are planned but not emitted.** Any value the allocator
   splits is demoted to memory. `Allocation::events` (`SpillKind::Load` /
   `Store` / `Move` / `Remat`) is computed and discarded.
5. **Phi coalescing is not used.** `regalloc::phi_edge_copies` and
   `resolve_parallel_copy` already resolve a phi web as a parallel copy;
   `emit_phi_copies` still does its own frame-word copy through the scratch slot.
   Phis remain unpromotable here.
6. **`Allocation::stack_slot` is unused.** The frame layout is still
   `plan_slots`'. The two are documented as interchangeable; that has not been
   tested, and after `release_deopt_pins` they are deliberately not.
7. **The flag is undeclared.** See *Enabling it*.
8. **No differential run.** The wiring has not been run against the Spring, H2 or
   WildFly suites with the flag on. It is off by default precisely because that
   evidence does not exist yet.

---

## Files

* `jit/src/ir_lower.rs` — the flag, `IR_LOWER_LS_XMMS`, `ir_lower_machine_model`,
  `plan_register_residency`, the `Lowerer` residency fields and accessors, the
  converted FP emission sites, the `lower_inner_with_scopes` call site.
* `jit/src/regalloc.rs` — `ir_op_is_call` widened to the helper-backed ops,
  `LiveModel::release_deopt_pins`, doc on `MachineModel::for_graph`'s limits.
* `jit/src/metrics.rs` — documentation only; `note_current_spills` /
  `note_current_reloads` already existed and now have a caller.
