# Wiring linear scan into `ir_lower`

> **Status banner, 2026-09-10.** This file describes the FIRST increment, when
> the path was XMM-only and default **off**. Both halves of that are stale:
> `CRATONVM_JIT_IR_LINEAR_SCAN` is **default ON** and declared, there is a
> **GP** file as well as an FP one (`regalloc::xmm_roles::IR_GP_LINEAR_SCAN`),
> and `ir-drop-home` / `ir-deopt-regs` / `ir-reg-authoritative` /
> `ir-phi-copy-regs` / `ir-drop-phi-home` have since made some home stores
> droppable. Read `docs/config/flag-inventory.md` for what is actually on.
>
> **Every stale sentence below now carries its correction inline**, dated, next
> to what it used to say — the first increment's reasoning is worth reading and
> its facts are not safe to quote. The one that mattered most is in *The
> register file*: this file's "no reference can be register-resident, because
> there is no GP register" argument is **gone**. By default the property now
> rests on `plan_register_residency` refusing `IrType::Ref`. Under the opt-in
> `CRATONVM_JIT_IR_REF_RESIDENCY` (default off) a reference **may** hold a GP
> register as an invalidated write-through copy.
>
> **Update, 2026-09-12.** *Safepoints*, *What clobbers what* and *Spill slots
> and the oop map* have been rewritten against `jit/src/regalloc.rs` and
> `jit/src/ir_lower.rs`, and carry no inline corrections: read those three
> sections as current.
>
> What is NOT stale is this file's central claim, and it is the reason to keep
> reading it: **write-through buys loads, not stores.** That ceiling was
> re-measured from the other side on 2026-09-10 by widening the GP file to the
> seven registers Win64 offers — residency rose, splits halved, and the code
> got **4.6% slower**. Measured again from a third direction the same day: on
> that loop the file is not merely un-widenable, it is a **net 1% cost at its
> current width** (`CRATONVM_JIT_IR_LINEAR_SCAN` off vs on, one binary, 0.0%
> floor). See
> `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`,
> which is also where the loops it DOES pay on are named.

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

Behind `CRATONVM_JIT_IR_LINEAR_SCAN`, `lower_inner_with_scopes` runs the
linear-scan allocator over an XMM file, verifies the result, cross-checks it
against `plan_slots`' independently computed live ranges, and uses what
survives as a **register read cache**: a value the allocation keeps in one
register for its whole life is copied into that register at its definition, and
its later reads become register moves instead of frame loads. Every value is
still written to its frame slot exactly as before. The frame image is
unchanged; only reads get cheaper.

*As written on the day: four XMM registers, default off. Both moved. The file
is XMM2–XMM7 (`regalloc::xmm_roles::IR_LINEAR_SCAN`, six) with a GP file
beside it, the flag is **default ON** since 2026-09-02, and write-through is
no longer universal — `ir-drop-home` and its relatives drop the home store for
a value no reachable frame state names. What did not move is the sentence that
matters: promoting a value the home store is still written for buys loads and
not stores.*

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

### Turning it OFF

```
CRATONVM_JIT_IR_LINEAR_SCAN=0
```

**It is default ON since 2026-09-02**, so the interesting direction is the kill
switch. **The flag is declared** — `types/src/flag_groups.rs` carries it as
`jit/ir-linear-scan`, so `-XX:` options and `flags::with_thread_overrides`
reach it as well as the environment variable. (As written, neither was true:
the flag was opt-in and undeclared, and `flags::runtime_var` fell through to a
live `std::env` read.) The unit tests deliberately depend on neither — they
drive a `#[cfg(test)]` thread-local override (`LsForce`) — which is why
declaring the flag did not silently turn them vacuous.

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

> **Where consequences 1 and 3 stand, 2026-09-10.** Both were true of the
> increment and neither is a standing property. `ir-deopt-regs` gave
> `build_deopt_points` a register bank, `ir-phi-copy-regs` gave `emit_copy_op`
> one, and `ir-drop-home` / `ir-drop-phi-home` then dropped the home store for
> values those two can describe — so the frame image is **no longer** complete
> at every instruction boundary, and the store side is **no longer** one per
> definition for every value. `Lowerer::slot_of` is what holds the line: a read
> of a dropped home REFUSES the compile rather than returning the stale word,
> so an unconverted reader costs coverage and cannot cost correctness. The one
> reader that reached a dropped home without asking produced a real miscompile
> (`internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`).
>
> `emit_safepoint_map` is the one on the original list that genuinely has NOT
> moved, and that is why `IrType::Ref` is still refused a register: the oop map
> names frame slots only.
>
> Consequence 3's *sentence* survives all of that, which is why this file is
> still worth reading — for any value whose home is still written, promoting it
> buys loads and no stores. Widening the GP file to test that from the other
> side made the code 4.6% slower:
> `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.

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
needed no edit. The GP half of the sentence stood until 2026-09-02 — writing
R12 would have corrupted the Rust caller — and `IR_GP_PROLOGUE_SAVED` is what
retired it: the prologue now saves the GP file too, dynamically, for the
registers a compile's residency plan actually handed out.

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

**As written this kept the wiring FP-only.** An `int` loop counter got nothing,
and that was already a *safepoint* question rather than a prologue one: a GP
file can hold references, so it has to discharge the oop-map obligation per
site instead of structurally. See
`docs/feature-designs/jit-machine-level-and-instruction-selection.md`, "Where
the safepoint / oop-map obligation lives".

> **Superseded 2026-09-02.** The GP file landed:
> `regalloc::xmm_roles::IR_GP_LINEAR_SCAN` — RBX and R12–R15; and on
> 2026-09-10 RSI/RDI joined it on Win64, where that ABI makes them
> callee-saved, behind `CRATONVM_JIT_IR_GP_WIDE` (default OFF, and off because
> it measured **slower**, not because it is unsoaked). So the paragraph below is
> the one part of this section a reader must not carry forward: the GC question
> is **no longer moot by construction**, because `RegClass::of(IrType::Ref) ==
> Gp` and there is now a GP file. It is discharged by TYPE instead —
> `plan_register_residency` refuses `IrType::Ref`, and
> `a_reference_is_never_register_resident` pins it. Whether the widening pays
> is answered in
> `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`:
> it does not, and the reason is the *"buys loads, not stores"* line below.

It also made the GC question moot by construction, which was worth having on
top of the argument below: `RegClass::of(IrType::Ref) == Gp`, and an XMM-only
file offers no GP register, so no reference could be register-resident at all.

---

## Safepoints

> **Verified against the source 2026-09-12** (`jit/src/regalloc.rs`,
> `jit/src/ir_lower.rs`). This section and the next two replace the first
> increment's text, which named four ops in `ir_op_is_call` and said no
> reference could ever hold a register.

The default rule is the one `docs/jit/linear-scan-regalloc.md` states:
**references stay in memory across every safepoint.** It holds three times over.
There is one opt-in exception, `CRATONVM_JIT_IR_REF_RESIDENCY` (token
`jit/ir-ref-residency`, **default off**, since 2026-09-09), described under
*Spill slots and the oop map* below.

1. **By type.** `plan_register_residency` matches each allocated register
   against its value's type. It admits `(RegClass::Xmm, Float | Double)` into
   `IR_LOWER_LS_XMMS` and `(RegClass::Gp, Int | Long)` into
   `IR_LOWER_LS_GPRS`. It admits `(RegClass::Gp, Ref)` **only** when
   `ir_ref_residency_enabled()`, and everything else counts as `skip_bank`.
   Pinned by `a_reference_is_never_register_resident` and
   `a_reference_is_never_promoted_into_the_gp_file`.
2. **By the allocator.** `allocate_linear_scan` drops a candidate when
   `live.is_ref[id] && !model.refs_may_cross_safepoints &&
   model.range_covers_safepoint(range)`, and `verify_allocation` proof 5
   re-checks the produced segments under the same knob.
   `ir_lower_machine_model` sets `refs_may_cross_safepoints =
   ir_ref_residency_enabled() && ir_ref_residency_cross_safepoint_enabled()`,
   which is `false` by default. `MachineModel::for_graph` alone always sets it
   `false`. Test: `refs_may_cross_safepoints_lifts_that_refusal_and_only_that_one`.
3. **By write-through.** A reference's home store is never dropped:
   `value_home_droppable` and `phi_home_droppable` admit only `Int` and `Long`.
   So the word `emit_safepoint_map` names still holds the reference at every
   safepoint, including under ref residency.

### What clobbers what: `ir_op_is_call`, `ir_op_is_safepoint`, `MachineModel`

Two private predicates in `jit/src/regalloc.rs` classify ops. Both are
deliberate over-approximations of what `ir_lower` emits:

| Op | `ir_op_is_safepoint` (collector may read the map) | `ir_op_is_call` (caller-saved registers destroyed) |
|---|---|---|
| `Call`, `New`, `NewArray` | yes | yes |
| `ConstString`, `ConstClass`, `LoadStatic` | yes | yes |
| `LambdaIntToDouble` | yes | yes |
| `InstanceOf`, `CheckCast` | yes | yes |
| `MonitorEnter`, `MonitorExit` | yes | yes |
| `ArrayStore(MemKind::Ref)` | yes | yes |
| `Guard` | yes | **no** |
| `Rem`, `Load(_)`, `Store(_)` | **no** | yes |

Why the three columns disagree, per the predicates' own doc comments:

* **`Guard` is a safepoint but not a call.** Its failure edge jumps to the shared
  deopt stub, which calls `ir_deopt_entry` and then runs the epilogue. It never
  returns into the body, so whatever it destroys is never read again. Counting
  it as a clobber would deny a register to every value in a bounds-checked
  loop.
* **`Rem`, `Load`, `Store` are calls but not oop-map publication sites.**
  `Op::Rem` on `Float`/`Double` calls `jit_frem` / `jit_drem`. `Op::Load(_)`
  calls `jit_getfield` whenever that helper is wired, which is every real
  compile. `Op::Store(_)` calls a putfield helper. Each is
  `MOV RAX, helper ; CALL RAX` and returns into the body. The predicate lists
  `Op::Rem` without a type test, so an integer remainder is also treated as a
  clobber.
* **Monitors and reference `ArrayStore` are both.** `ir_lower` publishes an oop
  map before each: a contended acquire can park for a whole collection, and
  `jit_aastore` allocates its `ArrayStoreException`. They were added to
  `ir_op_is_call` after a float/double interval in a caller-saved XMM was found
  not split across the helper.

`MachineModel::for_graph(graph, schedule, live, regs)` turns those predicates
into positions:

* for every scheduled node and terminator that has a position: a **safepoint**
  if `ir_op_is_safepoint`; a **clobber** of every `caller_saved` register in the
  file if `ir_op_is_call`; `Div` / `Rem` also destroy RAX and RDX, and
  `Shl` / `Shr` / `UShr` destroy RCX. Clobbers are filtered to registers that
  are actually in the file.
* for every block with a back edge (a successor `s <= b`): both the position
  before the outgoing edge and the edge position are **safepoints**, because
  the cooperative poll sits between them.
* `fixed` starts empty (`pin_entry_params` fills it) and
  `refs_may_cross_safepoints` is `false`.

`ir_lower::ir_lower_machine_model` adds what an op-derived model cannot see.
Those same two back-edge positions also become **clobbers** of the file's
caller-saved subset, because the poll's slow path (`jit_safepoint_slow_path`) is
an ordinary `extern "C"` call. Clobbers are merged per position through a
`BTreeMap`, because `MachineModel::clobbered_at` binary-searches a sorted list
with one entry per position. Test:
`the_machine_model_clobbers_the_back_edge_poll`.

Which registers that actually touches follows from the files
(`regalloc::xmm_roles`):

* **GP file** (`IR_GP_LINEAR_SCAN`): RBX and R12–R15, plus RSI/RDI on Win64.
  Every register in it is callee-saved on its target, so no call clobbers it.
  The Win64 widening is used only under `CRATONVM_JIT_IR_GP_WIDE` (opt-in,
  default off); otherwise the first `IR_GP_LINEAR_SCAN_NARROW` (5) are the file
  on every platform. The prologue saves those a plan hands out
  (`IR_GP_PROLOGUE_SAVED`).
* **XMM file** (`IR_LINEAR_SCAN` = XMM2–XMM7): on Win64, XMM6/XMM7 are
  callee-saved and saved by the prologue (`IR_PROLOGUE_SAVED`); XMM2–XMM5 are
  caller-saved. On System V all six are caller-saved and the save list is
  empty. So in practice a call or a back-edge poll clobbers XMM2–XMM5 (Win64)
  or XMM2–XMM7 (System V).

### Spill slots and the oop map

Three different "slot" notions meet here. Only the first one reaches machine
code or the oop map.

1. **Home slots from `ir_lower::plan_slots`**: the frame layout `ir_lower`
   actually emits, and the only slots `emit_safepoint_map` names.
   The colouring keeps two free lists and never moves a colour between its
   reference and non-reference lists (`plan_slots`' own comments call this step
   `assign_colors`; no function of that name exists).
   `verify_slot_colouring` re-derives the no-aliasing property rather than
   trusting it. Every phi, every value a
   `graph.safepoints` snapshot names, and every scalar-replacement field value
   is pinned (never shares a word). A `Ref` result of `Call`, `ConstString`,
   `ConstClass` or `LoadStatic` is `fresh_only`: it may donate a colour but never
   receive a recycled one, because the map published before that call already
   named the word.
2. **The allocator's own home colouring** (`ls_color_homes`, exposed as
   `Allocation::stack_slot` / `stack_slots`). It keeps three pools,
   `HomeClass::{Ref, Prim, Pinned}`, and `verify_allocation` proof 6 checks that
   no word mixes pools, so a word the map names can never come to hold a
   primitive. `ir_lower` does **not** read it; see *The deopt pins*.
3. **Planned spill events** (`Allocation::events`, `SpillKind::{Store, Load,
   Remat, Move}`). These are computed and not emitted. A value whose allocation
   has more than one segment is not promoted at all.

`Lowerer::emit_safepoint_map` is emitted immediately **before** the GC-capable
call, and before the node's result slot is allocated. It:

* stores the safepoint id into `[rbp - sp_id_slot_off]`;
* collects `ref_param_homes`, the prologue's homes for reference parameters,
  plus `node_slot` of every defined node typed `IrType::Ref`. If any such node
  has no slot or an offset beyond `i16::MAX`, the list is cleared and coverage
  is declared incomplete (fail closed);
* pushes those slots onto the shadow stack (`emit_shadow_push`);
* records an `OopMapEntry` with `frame_slot_offsets` = those slots,
  `moving_young_coverage_complete = coverable && (published ||
  slots.is_empty())`, `reg_oop_mask: None`, `local_oop_mask: None`,
  `non_oop_stack_slots` = the non-`Ref` colours (`prim_slot_offsets`), and
  `stack_marks_exact: true`.

The map therefore describes **home slots only**. A register copy is invisible to
the collector, which is safe for two reasons:

* **by default**, no reference is in a register (legs 1–3 above);
* **under `CRATONVM_JIT_IR_REF_RESIDENCY`**, the register is a write-through
  copy of a home the map does name. `Lowerer::invalidate_ref_residency(except)`
  clears the residency of every reference-typed register owner at each point
  where control can leave the body and come back, so the next read reloads the
  word the collector may have rewritten. `except` is the node the site itself
  just defined: a call's result is produced after the collection and is fresh.
  The doc comment on `ir_ref_residency_enabled` says why this invalidation asks
  an allowlist (`op_cannot_deopt`) rather than `ir_op_is_safepoint`: the backend
  also emits plain helper calls at ops that are not on the safepoint list
  (`Op::Load` reaching `jit_getfield`, `Op::Store` reaching a putfield helper).
  `ir_ref_residency_cross_safepoint_enabled` (default on within that flag;
  `CRATONVM_JIT_IR_REF_RESIDENCY_CROSS_SAFEPOINT=0` turns it off) decides
  whether such a range may cross a safepoint at all.

**Contrast with the single-pass backend.** `OopMapEntry` now has a register-file
half, `reg_oop_mask` (a bitmask over `x64::ALL_SPILL_GPRS`). The single-pass
backend's `emit_pre_safepoint_spill` writes its register file into the frame's
`reg_spill` region, a blind image a conservative scan can read. The IR tier sets
`reg_oop_mask: None` and emits no such image: its references are staged in frame
slots, so it has no register claim to make.

`vm/src/jit/conservative_roots.rs` consumes `frame_slot_offsets` (`i16`
displacements from RBP), plus `frame_layout`, `live_frame_hi` and
`sp_id_slot_off`. No code path rewrites a register on an evacuation, which is
why the reference exception above is built as a cache that is invalidated,
rather than as a register the collector is told about.

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
| A value is not `Float`/`Double`, has no home colour, or is a phi | that value is not promoted — **as written**; the bank now also takes `Int`/`Long`, and phis are admitted separately by `ir_phi_residency_enabled`. `IrType::Ref` is refused (`skip_bank`) unless `CRATONVM_JIT_IR_REF_RESIDENCY` is on (default off) |
| Two values share a register but `plan_slots` says their ranges overlap | **both** lose the register, counted as `demoted` |
| A clobber falls inside a value's `plan_slots` range | that value loses the register, counted as `demoted` |

Declining is always safe: a value with no register keeps the frame slot the
lowerer has always given it, and every read of it is the load it always was. A
non-zero `demoted` count is a compiler bug worth chasing — two liveness models
disagreeing — but it is not wrong code.

**Declining is still always safe, and it is worth saying why that survived home
dropping.** A home is dropped only for a value the plan promoted, so declining
happens strictly before there is anything to drop: a declined value keeps its
frame slot and every read of it is the load it always was. The order matters —
`set_residency` installs the plan, and only then does the droppability pass run
over it.

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

With the flag off — as written, the default; since 2026-09-02, the kill switch
— both stay `NotMeasured`. That is the truth, not a gap: the lowerer allocates
no registers, so there is no register↔memory transition to count, and a
reported `0` would be indistinguishable from "an allocator ran and spilled
nothing". Keeping those two distinguishable is the entire reason `Measured<T>`
exists.

---

## What is **not** done

Listed so nobody reads a green test suite as a finished item. Written for the
first increment; each item carries where it stands as of **2026-09-10**.

1. **Not measured on a benchmark.** No CratonBench run, no before/after on an FP
   kernel. The claim here is "correct and enabled", not "faster". The report's
   15–45% estimate is for a *general* allocator; this one covers four XMM
   registers and no store elimination, and should be expected to deliver a small
   fraction of it.

   **Since: measured, repeatedly, and the answer is the one this file
   predicted.** The GP half was A/B'd on the tier-inversion loop and again with
   the file widened; residency rose and the code did not get faster. See
   `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.
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

   **Since: done, 2026-09-02** — `IR_GP_LINEAR_SCAN` and `IR_GP_PROLOGUE_SAVED`.
   It took neither of the two shapes above. `IrType::Ref` is simply **refused**
   by `plan_register_residency`, so there is no per-safepoint publish plan and
   `OopMapEntry` is unchanged. Reference promotion is still not done and still
   needs the register bank item 3 names.

   **Since 2026-09-09, opt-in:** `CRATONVM_JIT_IR_REF_RESIDENCY` (default off)
   admits a reference into the GP file without a register bank. The home is
   still written, and `invalidate_ref_residency` drops the copy wherever a
   collector could have run. See *Spill slots and the oop map*.
3. **Stores are not eliminated.** Write-through is what makes the safepoint,
   deopt and phi arguments hold without touching those paths. Dropping the home
   store means teaching `emit_safepoint_map`, `build_deopt_points` and
   `emit_phi_copies` to read a register — i.e. a register bank in the oop map
   and in the deopt frame reconstructor, which `vm/src/jit/conservative_roots.rs`
   does not have.

   **Since: partly, and this is the item everything else waits on.**
   `ir-deopt-regs` gave the deopt frame reconstructor a register bank,
   `ir-phi-copy-regs` let the edge copies read one, and `ir-drop-home` /
   `ir-drop-phi-home` / `ir-reg-authoritative` drop the home store for an `Int`
   or `Long` that no REACHABLE frame state names. The **oop map** still has no
   register bank, which is why `IrType::Ref` is still refused. What actually
   got dropped on a given compile is `[ir-ls] homes: dropped_values=…`.
4. **Splits and reloads are planned but not emitted.** Any value the allocator
   splits is demoted to memory. `Allocation::events` (`SpillKind::Load` /
   `Store` / `Move` / `Remat`) is computed and discarded. **Still true**; the
   `split_or_spilled` skip cause in the census is what it costs.
5. **Phi coalescing is not used.** `regalloc::phi_edge_copies` and
   `resolve_parallel_copy` already resolve a phi web as a parallel copy;
   `emit_phi_copies` still does its own frame-word copy through the scratch slot.
   Phis remain unpromotable here.

   **Since: phis are promotable** (`ir_phi_residency_enabled`,
   `ir_phi_copy_regs_enabled`), and `resolve_parallel_copy` is what
   `emit_copy_op` drives. This is the item whose closing produced the one silent
   miscompile in the area — a self-copy publishes nothing, so a phi whose home
   had been dropped had no word left to reload from
   (`internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`).
6. **`Allocation::stack_slot` is unused.** The frame layout is still
   `plan_slots`'. The two are documented as interchangeable; that has not been
   tested, and after `release_deopt_pins` they are deliberately not. **Still
   true, and deliberately so** — see the contract note in *The deopt pins*.
7. **The flag is undeclared.** See *Turning it OFF*. **Since: declared**, as
   `jit/ir-linear-scan` in `types/src/flag_groups.rs`.
8. **No differential run.** The wiring has not been run against the Spring, H2 or
   WildFly suites with the flag on. It is off by default precisely because that
   evidence does not exist yet. **Since: run, and the default moved on
   2026-09-02** — the regression suite is green with the file on (88/88), and
   the bugs it did surface are recorded as fixed rather than as reasons to turn
   it back off.

---

## Files

* `jit/src/ir_lower.rs` — the flag, `IR_LOWER_LS_XMMS`, `ir_lower_machine_model`,
  `plan_register_residency`, the `Lowerer` residency fields and accessors, the
  converted FP emission sites, the `lower_inner_with_scopes` call site.
* `jit/src/regalloc.rs` — `ir_op_is_call` widened to the helper-backed ops,
  `LiveModel::release_deopt_pins`, doc on `MachineModel::for_graph`'s limits.
  Also, as of 2026-09-12: `ir_op_is_safepoint`,
  `MachineModel::refs_may_cross_safepoints`, `HomeClass` / `ls_color_homes`,
  `SpillKind`, and `xmm_roles` (`IR_LINEAR_SCAN`, `IR_PROLOGUE_SAVED`,
  `IR_GP_LINEAR_SCAN`, `IR_GP_LINEAR_SCAN_NARROW`, `IR_GP_PROLOGUE_SAVED`).
* `jit/src/ir_lower.rs`, also: `emit_safepoint_map`, `plan_slots` /
  `verify_slot_colouring`, `value_home_droppable` / `phi_home_droppable`,
  `invalidate_ref_residency`, `ir_ref_residency_enabled` /
  `ir_ref_residency_cross_safepoint_enabled`.
* `jit/src/metrics.rs` — documentation only; `note_current_spills` /
  `note_current_reloads` already existed and now have a caller.
