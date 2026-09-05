# A strided loop's stored VALUE was computed once and reused

**Status:** ✅ RESOLVED 2026-09-05. Two causes closed, the one open item
this page carried (cause 1's missing kill switch) closed with them, and an
adjacent hole in the same bitmask closed on the way out. **Retired from**
`docs/known-issues/jit/`.
**Reproducers:** `test_classes/jit/OsrStridedValue.java`,
`test_classes/jit/OsrStridedValueMin.java` — self-contained, no GPU — plus
`test_classes/jit/StridedInvariantValue.java`, which reaches the compiled
tier by INVOCATION COUNT rather than OSR and so pins the "never
OSR-specific" claim below instead of leaving it an assertion.
**Found:** 2026-09-04, chasing what looked like a GPU offload defect. It
was not one.

The one thing this page handed on — flipping
`CRATONVM_GPU_JIT_GATE_CALLERS=hook`'s default, which the fix unblocked — was
taken up and landed by a sibling lane the same day. Nothing is outstanding.

## The symptom

```java
for (int r = 0; r < 12; r++)
    for (int i = 0; i < n; i += 1024) a[i] = i + r;
```

```
--nojit / HotSpot   11 1035 2059 3083 4107
cratonvm (JIT)      11   11   11   11   11
```

The **addresses are right** and the **values are frozen at the first
iteration's `i`**. Anything loop-invariant in the same body (`a[i] = r`)
stays correct, which is what made it read as an addressing bug.

## Cause 2 — the real one: `wide iinc` was invisible to LICM

`find_modified_locals` (`jit/src/x64/escape_analysis.rs`) builds the
bitmask of locals a loop writes. It had an arm for every narrow store
form and for `iinc` (0x84) — and **no arm for `wide` (0xc4) at all**.
Those bytes fell through to the length-only default: the walk stayed
aligned, so nothing looked broken, and the local was never marked
modified.

javac emits `wide iinc` whenever the increment does not fit in a signed
byte. `i += 1024` is exactly that. So the loop's **induction variable
read as loop-INVARIANT**, and `find_arith_loop_hoists` hoisted `i + r`
into the pre-header, where it is computed once and every iteration
replays the slot.

That is why the trigger looked so arbitrary:

| shape | why |
|---|---|
| `i += 1` correct, `i += 1024` wrong | plain `iinc` vs `wide iinc` |
| `a[i] = 5` correct | constant — no arithmetic run to hoist |
| `a[i] = i` correct | a bare load is not a hoistable run |
| `a[i] = i + r` wrong | a run over a "loop-invariant" local |
| `a[i] = r` correct | genuinely invariant, correctly hoisted |

The bitmask only ever DISABLES a hoist, so over-marking is the safe
direction; the fix marks the local for `wide iinc` and for every `wide`
store. `find_modified_locals` also feeds the `aaload`, FP and
array-length hoisters, so all four shared this blind spot.

**This was never OSR-specific.** `CRATONVM_JIT_OSR=0` appeared to fix it
only because these fixtures reach the hot loop through OSR and nothing
else compiles the method. Any hot loop with a `wide iinc` induction
variable and a hoistable integer expression over it was affected.

Kill switch: `CRATONVM_JIT=-arith-licm` (pre-existing, and spelled
`CRATONVM_DISABLE_ARITH_LICM=1` before the flag-group rename) turns the
hoister off entirely. It is the arm that isolates the cause:

```
                               the stride-1024 arm of StridedInvariantValue
HotSpot                        7 1031 2055 3079 4103 5127 6151 7175
cratonvm --nojit               7 1031 2055 3079 4103 5127 6151 7175
cratonvm before                7    7    7    7    7    7    7    7   <- wrong
  … CRATONVM_JIT_OSR=0         7    7    7    7    7    7    7    7   <- still wrong
  … CRATONVM_JIT=-arith-licm   7 1031 2055 3079 4103 5127 6151 7175
cratonvm after                 7 1031 2055 3079 4103 5127 6151 7175
```

`fill` there is INVOKED hot, not entered by OSR, so the middle two rows are
the whole "never OSR-specific" argument in two lines. The fixture's own
stride-1 arm prints the correct row in every one of those configurations.
`CRATONVM_DBG_JIT_GEN=1` names the hoist directly —
`[JIT_GEN] arith-LICM hoists=1 runs=[(9, 12, 3)]`, bytecode pcs 9..12,
which is `iload_3 ; iload_2 ; iadd` — and prints nothing after the fix.

## Cause 1 — also fixed: OSR entry with a live expression stack

`bytecode_walk.rs` marks a pc OSR-ineligible for hoisted-loop interiors,
synthetic guards and handler-only pcs. It did not require the abstract
operand stack to be **empty**. Entering mid-expression means the
prologue materialises the pending operands for the entering iteration,
and the loop can never recompute them, because the pushes live above the
back-edge target. `operand_stack_live` now joins that rejection set.
HotSpot has the same rule.

**Honest note on its evidence.** This landed first, and the reason given
was that it fixed the single-store reproducer. With cause 2 understood,
that improvement is attributable to cause 2's hoist no longer being
reachable from the entry pc the rule moved OSR to — so cause 1 rests on
the argument, not on a demonstrated failure of its own. It is kept
because the argument is sound and `osr.rs` states the same invariant
from the other side (`osr_entry_native[header]` points *before* a
hoisted preheader precisely so a cold OSR entry runs it).

**Asked and answered.** With cause 2 fixed, every reproducer here is
correct whether the rule is on or off. So it has no demonstrated failure
of its own, and it did cost something: a versioning OSR test asserted
the behaviour it removed (`c82154c4d`). It stays on — a soundness rule
HotSpot also enforces, costing nothing measurable because a refused
mid-expression pc is re-reached at the header a few bytecodes later.

**Kill switch: `CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1`** (2026-09-05).
This was the last open item on the page — a default-on codegen rule with
no way to turn it off. `x64::osr::osr_empty_stack_entry_enabled` is a
presence-parsed `OnceLock` read (`=0` still turns the rule OFF), declared
in `types/src/flag_groups.rs` as the JIT token `osr-empty-stack-entry`, so
`CRATONVM_JIT=-osr-empty-stack-entry` spells it too and `flags` reports it
when somebody leaves it on. The point is not that the rule is doubtful; it
is that re-opening the question should not require rebuilding the VM.

## The adjacent hole the same audit turned up

`find_modified_locals` recorded only the LOWER slot of a `long`/`double`
store. A category-2 store writes two, and the upper one — dead to the
reader, written by the store all the same — was invisible to every
consumer of the bitmask, exactly as `wide` had been.
`loop_analysis::modified_locals_strict`, the reviewed twin of this
function, has always marked it, and says why in its doc: an invariance
check built on a set that quietly forgot a write is not a check.

Two tests in `jit/src/x64/tests.rs` had pinned the consequence the wrong
way round. `p87_fp_loop_hoist_detection` and `p87_fp_hoist_double_and_float`
both asserted that a `dload_1` is hoistable out of a loop whose body
contains `dstore_0`, the second with the comment *"this modifies 0, but
doesn't affect 1"*. `dstore_0` writes slots 0 AND 1. Both fixtures were
written from what the implementation did, and both therefore certified an
unsound hoist — the same failure mode as the `find_array_len_hoists` doc
one section up, where a shared analysis's blind spot was written down as
a property instead of being fixed.

Fixed on both sides, because they have to agree: the store side marks the
high half (`find_modified_locals`), and the load side refuses a `dload k`
when the loop writes k+1 (`find_fp_loop_hoists`). Both directions can only
REFUSE a hoist, never admit one.

Stated honestly: javac cannot emit the shape. After `dstore_0` slot 1
holds TOP, so a later `dload_1` fails verification unless something
re-stores the slot first — which marks it anyway. So this bought no
measurable behaviour, only agreement between two masks that disagreed and
the removal of two tests that said the wrong thing out loud.

## What it was NOT — tested and refuted

| suspect | switch | result |
|---|---|---|
| GPU offload / the residency cache | no `--gpu` at all | still wrong |
| the compiled-tier array barrier | `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` | still wrong |
| IR loop-invariant code motion | `CRATONVM_JIT_LICM=0` | still wrong |
| IR loop unrolling | `CRATONVM_JIT_UNROLL=0` | still wrong |
| the single-pass native unroller | `CRATONVM_DISABLE_UNROLL=1` | still wrong |
| IR affine strength reduction | `CRATONVM_JIT_REASSOC=0` | still wrong |
| OSR frame-slot seeding / dead-local masking / single-pc | four switches | still wrong |
| "three stores in one body" | a three-store fixture | **correct** — refuted |
| "the inner loop starts at the outer IV" | `i = 0` and `i = r` | both wrong — refuted |
| **single-pass integer LICM** | `CRATONVM_DISABLE_ARITH_LICM=1` | **correct** |

Two readings of mine were wrong on the way and are worth keeping visible.
The disassembly showed the value expression sitting **above the loop's
back-edge target**, and I read that as a label-placement bug. The
observation was right — a pre-header is exactly where a hoist goes — and
the interpretation was wrong. Earlier still, I read two arms
(`ARRAY_WRITERS=refuse` passes, `MIN_WORK_GIVEUP=0` fails) as implicating
the GPU residency barrier; both were true and both were also consistent
with something else, because both additionally stop the method being
compiled. **When every discriminating arm shares a side effect, it is not
discriminating.**

## Why no suite caught it

`bench-gpu/runtime-stress.sh` ran three arms and **none compiled the
method**: HotSpot, `cratonvm --nojit` (interpreted by construction), and
`cratonvm --gpu`, where `runtime::offload_jit_gate` refused every
scenario in the file — they all write a primitive array or call an
offload-eligible kernel.

A fourth **compiled CPU arm** was added 2026-09-04 and is what reported
it. The lesson outlives the defect: a gate that keeps methods interpreted
removes them from every differential suite that reaches the compiled tier
only through that gate.

## What this unblocks

`CRATONVM_GPU_JIT_GATE_CALLERS=hook`
(`../../known-issues/perf/gpu-compiled-caller-offload-hook-20260904.md`)
was held opt-in
for exactly one reason: it failed `cache_coherence`. That failure was
THIS defect — the hook merely became the first thing that ever compiled
the method. With `wide iinc` visible, `runtime-stress.sh` passes **all
seven scenarios under `hook`**, as does `jit-writer-stale.sh`.

So the 27-42x that mode is worth is unblocked — and **`CompiledHook` is
the default since 2026-09-05**, flipped by a sibling lane once this fix
landed, with `bench-gpu/gate-overbroad.sh`'s arm C rewritten in the same pass
(it was vacuous under `hook`, which disables the caller-blocking the arm
toggles). That page carries the measurement and the rewrite; this one has no
hand-off left.
