# `osr-entry-unresumable-exit` — FIXED 2026-08-03, both causes

**Status:** FIXED. Across the regression-suite corpus the count of
`osr-entry-unresumable-exit` refusals went **110 → 0**, and a hot loop in a
rarely-invoked method now leaves the interpreter whether or not it contains a
call. Retired from `docs/known-issues/jit/`.

## The rule that made one bad snapshot fatal

`CompiledMethod::osr_exit_policy` is an **artifact-wide veto**: it walks every
deopt point in the artifact and, if any one reconstructs a frame that cannot be
resumed, refuses OSR entry at **every** pc of that method. That is the correct
rule — entering, committing loop iterations, then bailing somewhere unresumable
is the replay bug the OSR-exit design exists to prevent — so the fix was never
going to be to weaken it. It had to become possible to *describe* the frames.

Two independent causes produced undescribable frames.

## Cause 1 — a map at every bytecode boundary

`deopt-osr` Step 7 emits an OSR-exit map "at a loop-boundary bci". It did not:
the condition it sat under was `pc < osr_entry_native.len()` minus the
LICM-hoisted interiors — essentially every pc. `StaticFieldProbe.control`
recorded fourteen maps, one at bci 1 (`lstore_1`) with a `long` on the operand
stack. Mid-expression snapshots have partially-built stacks, and (cause 2) a
stack entry had no width source, so that one entry vetoed the method.

**Fixed** by emitting the map only at back-edge targets — which is the only
place the metadata is ever consulted (`osr_entry_frame_state(entry_pc)` looks it
up at the entry bci; the sole reason-7 stub emitter is the Step-8 test trigger,
which picks the lowest loop header). The header set is derived from the same
`code` slice the walk iterates, so it stays in the walk's coordinate space when
the bytecode loop rewriter is armed.

Measured on `StaticFieldProbe` warmed to only 200 invocations, so OSR is the
*only* route into compiled code, 2M iterations:

| rung | before | after | HotSpot |
|---|---|---|---|
| control (no field) | 163.04 | **1.20** | 0.76 |
| `static final` REF | 750.32 | **2.52** | 1.15 |
| instance field | 444.63 | **2.06** | 1.23 |

136x on the control rung, checksum identical to HotSpot.

Regression test: `osr_exit_maps_are_emitted_at_loop_headers_only` — with the
gate reverted it fails with `left: [0, 1, 2, 3, 5, 6]`, `right: [2]`.

## Cause 2 — the operand stack had no width source

**This was the one that mattered on real code.** With cause 1 fixed, the corpus
refusal count did not move at all: 110 before, 110 after. Every one of them was
a `ReceiverTypeChanged` call-site guard — snapshotted before the argument pops,
so the stack still holds `[.., receiver, args]` — blocked by exactly one entry:

```
RJitGc.main entry_pc=7 (deopt point at bci 57 (ReceiverTypeChanged)
  reconstructs an unresumable frame: stack 0 (Unsupported) of 1)
```

bci 57 is `invokestatic Double.doubleToLongBits(D)J`; the blocking entry is the
`double` argument. Note the shape: bci 57 is *outside* the loop being entered,
and it still refused entry at pcs 7, 85, 152, 198, 209 and 248.

The cause: locals have a width source (`local_kinds` plus the per-bci
reaching-kind refinement); the stack had none. So `build_and_record_deopt_point`
fell back to a per-METHOD gate — if the method touches a `long`/`float`/`double`
anywhere, every non-oop stack entry is `FrameValue::Unsupported`, because an
8-byte frame slot holding a cat-1 `int` and one holding a cat-2 `long` are
indistinguishable and guessing wrong truncates a `long` on resume.

**Fixed** by giving the stack a real type model: `jit/src/x64/stack_kinds.rs`, a
forward abstract interpretation over the bytecode producing, per bci, the kind
of every entry of the same *compact* operand stack the backend simulates (one
entry per value, cat-2 included — which is what makes the result
index-alignable with `Compiler::stack`).

### Why a wrong answer here cannot reach a resume

A wrong kind is silent wrong values — exactly what the coarse fallback existed
to prevent. Four independent properties, not one:

1. **It may only ever upgrade.** The consult site asks the analysis only in the
   branch that previously produced `Unsupported`. Every encoding the old code
   produced, it still produces.
2. **`Unknown` is not a guess.** An opcode whose depth effect is known but whose
   result type is not (`ldc` of an int-or-float constant, `ldc2_w` of a
   long-or-double) pushes `Unknown`, which the consult site treats exactly as
   before. Only *depth* ambiguity poisons.
3. **Poison is total.** An unmodelled opcode, `jsr`/`ret`, a category-dependent
   `pop2`/`dup2` over an `Unknown` top, or a merge of two different depths
   abandons the state — no answer for that pc or anything downstream — rather
   than a plausible one. A pc poisoned *after* it was first published has its
   earlier answer withdrawn.
4. **The consumer re-checks, twice.** The vector is used only when its length
   equals the emitter's live stack depth AND every entry's ref-ness agrees with
   the emitter's own oop mark. Depth is genuinely independent (the analysis
   derives it from the JVMS stack effects, the emitter from running its opcode
   handlers); the oop marks are maintained for the GC, so they are a second
   opinion with a different provenance, and they catch an off-by-one that
   happens to preserve depth.

### What it bought

`VirtOnlyProbe` at 200 invocations — every rung's loop contains a call, so
before this none of them could OSR:

| rung | cause-1 fix only | + typed stack | HotSpot |
|---|---|---|---|
| `arith` (no call) | 0.77 | 0.76 | 0.75 |
| `invokestatic` | 272.44 | **4.22** | 1.69 |
| `invokevirtual` | 374.19 | **6.78** | 1.77 |
| `invokeinterface` | 376.38 | **6.99** | 0.74 |

54-65x, checksums identical to HotSpot on all rungs.

Corpus-wide `osr-entry-unresumable-exit` refusals over 13 regression-suite
classes: **110 → 110 → 0** for (pre-fix / cause-1 only / both).

### Direct correctness evidence

The suites exercise *entering* OSR. What this change really alters is whether a
frame can be *rebuilt*, so the load-bearing test forces real OSR exits through
the newly-describable snapshots (`CRATONVM_OSR_EXIT_AFTER=N` bails on the N-th
reach of a loop header, carrying genuinely JIT-advanced state into the
interpreter):

| program | N | result |
|---|---|---|
| `JitDifferential` (74 observations) | 1, 3, 17 | byte-identical to HotSpot |
| `StaticFieldProbe` | 1, 5, 33 | checksum identical to HotSpot |
| `VirtOnlyProbe` | 1, 5, 33 | checksum identical to HotSpot |

Plus: jit 1869 · vm `--lib` 2376 · `jit_interp_differential` with HotSpot
triangulation · `interpreter_tests` 924 · `differential` 14 ·
`jit_local_exception_handler_tests` 16 · `regression-suite` 22/22 HotSpot-diffed.

## Diagnostics worth keeping

* **The refusal names its blocking slot**: `stack 1 (Unsupported) of 3` rather
  than "reconstructs an unresumable frame". An unsupported STACK entry means the
  operand stack had no width source at that bci; an unsupported LOCAL means its
  kind or liveness was unknown. Those are different defects, and the old message
  could not tell them apart — it cost an instrumented rebuild to learn which one
  this was. `deopt::first_unresumable_slot`.
* **`CRATONVM_DBG=stack-kinds`** prints, per compile, how many pcs the analysis
  answered for, and per snapshot the emitter's depth, the analysis' vector, and
  whether the agreement checks accepted it. That is what caught the first
  attempt at wiring this in: placed too early in the driver, three of its five
  metadata inputs were still empty, so it answered nothing — the suites stayed
  green and the refusal count did not move at all, which is the signature of an
  analysis that answered *nothing* rather than one that answered *wrong*. Order
  matters: `analyze_stack_kinds` runs immediately before the walk.

## What remains

The analysis declines (poisons) on `jsr`/`ret` and any opcode not in its table.
Those cost precision, not correctness: such a method keeps the old coarse
encoding and may still be refused OSR. None appeared in the corpus. If a
workload ever shows one, the fix is to add the arm — with the JVMS clause quoted
next to it, as the existing arms do.

**Update 2026-08-18.** The category-dependent `dup2` family (`dup_x2`, `dup2`,
`dup2_x1`, `dup2_x2`) was on that list and is no longer: every form is decided
by categories this analysis already tracks, so modelling them costs neither
precision nor correctness, and `Unknown` in a deciding position still poisons.
The prompt was not OSR precision at all — the single-pass `dup2_x2` codegen arm
needed a **second-entry width oracle**, and this analysis turned out to be one.
See fixed-suite-bugs/jit/dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md,
which is also where the argument for using a snapshot oracle to pick a codegen
shape is written down.

## Reproduction

```bash
# OSR-only measurement: warm by ITERATIONS, so the only route in is OSR.
# The control rung tells you which tier actually ran.
cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 2000000

# Every refusal, with the slot that blocked it:
CRATONVM_DBG=jitc cratonvm --java-home <jdk25> -cp regression-suite/build RJitGc \
  2>&1 | grep 'unresumable frame'

# Force real OSR exits through the snapshots and diff the output:
CRATONVM_JIT_THRESHOLD=1 CRATONVM_OSR_EXIT_AFTER=3 cratonvm -c vm/tests/resources \
  cratonvm.JitDifferential
```

## Where to look

* `jit/src/x64/stack_kinds.rs` — the analysis, its safety argument, its tests.
* `jit/src/x64/deopt_stubs.rs` — `analyze_stack_kinds` (input adapter) and
  `build_and_record_deopt_point`'s operand-stack loop (the consult site).
* `jit/src/x64/bytecode_walk.rs` — the Step-7 emission site and its header gate.
* `jit/src/lib.rs` — `CompiledMethod::osr_exit_policy`, `osr_entry_frame_state`.
* `jit/src/deopt.rs` — `frame_state_is_resumable`, `first_unresumable_slot`.
* `docs/jit/on-stack-replacement.md` — the refusal taxonomy.
