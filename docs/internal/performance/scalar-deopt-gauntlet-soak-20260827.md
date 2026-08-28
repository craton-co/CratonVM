# `CRATONVM_SCALAR_DEOPT` on the gauntlet: green, and green means it never ran

## Status

**SOAKED, 2026-08-27. NOT flipped, and the soak is the argument for not flipping.**

The flag is safe — no arm of this soak found a behaviour difference. It is also
**inert**: on the sampled gauntlet it changes nothing, and its own producer emits
nothing even on the probe built to exercise it. A default flip on this evidence
would be recording a soak that never executed the feature.

## What the flag is supposed to do

Guard-surviving scalar replacement. With `CRATONVM_SCALAR_DEOPT` (+
`CRATONVM_DEOPT_REAL`, default-on) the IR lowerer emits a
`FrameValue::VirtualObject` for a scalar-replaced object that is live at a deopt
point, so a precise resume can re-materialize it instead of falling back to a
whole-method re-run. Two halves:

* **Producer** — `ir_lower::frame_value_for_object` writes the recipe.
* **Consumer** — `vm::runtime::deopt_materialize::materialize_virtual_objects`
  rebuilds the object when a deopt lands on a frame naming it.

There is a third, less obvious effect, and it is the only one that was live in
any measurement here: the descriptor's *existence at compile time* is what lets
`plan_scalar_replacement` elide an allocation that a safepoint snapshot names.
That is where the per-voxel 10.9x came from
(`fixed-bugs/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`).

## The engagement instrument, and why it had to come first

The consumer had no counter. A workload can run the producer's whole gate chain
and never reach `materialize_virtual_objects`, so "the suite is green" says
nothing about the half that can hand the interpreter a wrong object. One line
per materialization was added under the flag's existing debug switch, so
`grep -c` is the whole instrument.

That instrument is what turned this from a soak into a finding.

## Measured

Same binary throughout, flag A/B, `perf/scalar-deopt-gauntlet-soak-20260827`.

### Netty — 200 classes, and the flag is inert

Netty's pass set is `testlist.txt` minus `netty-nonpassed-latest.txt`, 690
classes; the first 200 were run in each arm, 5 shards, one binary.

| arm | `@@RESULT` rows | classes with `failed>0` |
|---|---:|---:|
| off | 191 | 60 |
| on | 189 | 60 |

Two caveats the raw numbers carry, neither of them about the flag. The row
counts differ because four classes produced no `@@RESULT` in one arm or the
other (a hang or crash under 5-way sharding on a host already at load ~9). And
60 of 200 have failing tests in **both** arms — the committed
`netty-nonpassed-latest.txt` is stale, so the derived "pass set" is not one.
Both effects are arm-symmetric and cancel in the differential.

Verdict differences on the classes both arms reported: **two**, and they move in
opposite directions, which is the signature of noise rather than a regression.
Re-run serially (1 shard), three reps per arm:

| class | off | on |
|---|---|---|
| `MultiThreadIoEventLoopGroupTest` | fail, fail, pass | pass, pass, pass |
| `SingleThreadEventLoopTest` | fail, pass, pass | fail, pass, pass |

Both are flaky, both flake in the **off** arm, and the first one fails *only*
there. Neither is attributable to the flag.

The engagement census over the same 200 classes, with `CRATONVM_JIT_IR_INLINE=1`
added so the flag had every chance:

| counter | value |
|---|---:|
| `[cratonvm-scalarnew]` reports (allocation-bearing IR compiles) | **30 392** |
| allocations REPLACED | **0** |
| `emit VirtualObject` (producer) | **0** |
| `MATERIALIZE` (consumer) | **0** |

The refusal census over the 40-class run says why, and it is not a gate or a
defect — the allocations genuinely leave their methods:

| refusal | count |
|---|---:|
| `Escapes(...)` | 3 381 |
| `ArrayNotReplaceable(...)` | 302 |
| `REPLACED` | 0 |

With nothing scalar-replaced there is nothing to describe, nothing extra to
elide, and no way for the flag to change behaviour. The near-identical pass sets
are a **consequence** of that, not evidence of correctness.

### Hibernate — a third workload, same answer

`hql.ASTParserLoadingTest` against a fresh MySQL 8.0.46 database per arm:

| arm | result | wall | `[cratonvm-scalarnew]` reports | REPLACED |
|---|---|---:|---:|---:|
| off | 104 ok, 0 failed | 1049 s | — | — |
| on (+ `IR_INLINE`) | 104 ok, 0 failed | 949 s | **2 577** | **0** |

### The regression suite — green in all three arms

| arm | result |
|---|---|
| default | 72 passed, 0 failed |
| `CRATONVM_SCALAR_DEOPT=1` | 72 passed, 0 failed |
| `CRATONVM_SCALAR_DEOPT=1 CRATONVM_JIT_IR_INLINE=1` | 72 passed, 0 failed |

One run in an earlier pass reported `RSimpleDateFormatZone` failed under the
flags. It does **not** reproduce: 3/3 in isolation on both arms, and 72/72 on
the full suite on both arms afterwards. The host was carrying an unrelated load
average of ~9 at the time. Recorded because a one-off failure that is dropped
without being chased is how a real one gets dismissed later.

### The positive control — the producer emits nothing even here

This is the result that decides the question. `probes/VoxelAlloc2.java` is the
one workload known to scalar-replace (both of `new Short2()`'s allocations), and
with the flag on it still emits **zero** descriptors:

```
[cratonvm-scalarnew] sweepVolume: scalar-replaced 1/2 alloc(s)   -> then 1/1
[DBG_SCALAR_DEOPT] resolve bci 52 deopt_block=None sr_objects=2 matching_slots=[60]
[DBG_SCALAR_DEOPT] bail new 60: no deopt block for bci
```

The object is found in the snapshot (`matching_slots=[60]`) and the recipe is
refused for want of a deopt *block*. `deopt_block_for_bci` answers `Some` only
for a bci carrying an `Op::Guard`, `Op::Div` or `Op::Rem` — those are the only
deopt points the IR tier emits — and none of this probe's snapshot bcis is one.

So a descriptor is emitted only where a snapshot bci and a guard/div/rem bci
COINCIDE. That intersection is empty on every workload measured here, including
the one written to exercise the feature. `can_deopt_resume` is therefore never
set (it requires `count_virtual_objects > 0`), the artifact never offers a
precise resume, and every deopt takes the whole-method re-run it always did.

That is also why the flag is safe: the elision it permits is not backed by a
recipe that is ever consulted, because the frames that would consult it do not
exist.

## Why this is a "do not flip", not a "flip"

1. **Flipping it alone is a no-op, measured.** On the voxel probe with
   `CRATONVM_JIT_IR_INLINE` off, the target arm is 479 ns/voxel with the flag
   and 478 without. Every gauntlet counter above is zero. A default flip would
   change no observable behaviour on anything measured.
2. **The green is vacuous.** Identical pass sets across a run in which the
   feature executed zero times is not evidence that the feature is correct. It
   is evidence that it did not run. Flipping on it would put a soak in the
   record that tested nothing — the exact failure mode the flag-flip discipline
   in `docs/feature-designs/activate-ir-optimizer.md` exists to prevent.
3. **The consumer was not reached once in this soak.**
   `materialize_virtual_objects` ran zero times. Be precise about what that does
   and does not say: `scalar_deopt_enabled()` is read only in `jit/src/lib.rs`,
   so this flag gates the **IR-tier** producer alone. The single-pass producer
   (`x64/deopt_stubs.rs::sr_virtual_object_state`) is UNGATED and was validated
   at runtime in the Phase B backport (`0db926ed`, via
   `CRATONVM_DEOPT_EAGER_BCI`). So the consumer is not dead code in general —
   but the half this flag switches on emitted nothing anywhere measured, which
   is the half a default flip would be asserting confidence in.

The flag that would actually pay is the **pair** with `CRATONVM_JIT_IR_INLINE`,
which is where the 10.9x lives. Its blocker is that flag's own unpaid soak, not
this one's.

## What would change the answer

* A workload where the IR tier scalar-replaces at all. Netty's 11 237 reports
  produced zero, and the refusals are honest escapes — so the first question is
  not "is the flag safe" but "why does IR-tier EA replace nothing in server
  code". That is the finding worth chasing.
* An end-to-end exercise of the consumer, most plausibly via
  `CRATONVM_DEOPT_EAGER_BCI` pointed at a straight-line bci where a
  scalar-replaced object is live, which is what that lever was added for.

## Reproducing

```bash
# engagement census over a netty slice (both flags on, so the flag has every chance)
cd apps/netty-suite-runner
tr -d '\r' < netty-nonpassed-latest.txt > /tmp/np.txt   # the list is CRLF
tr -d '\r' < testlist.txt > /tmp/tl.txt
grep -v -x -F -f /tmp/np.txt /tmp/tl.txt > /tmp/passed.txt   # 690 classes
CRATONVM_SCALAR_DEOPT=1 CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_SCALAR_NEW=1 \
  ./run-netty-suite.sh --list /tmp/passed.txt --count 40 --bin <cratonvm> --out /tmp/eng --shards 4
grep -rh cratonvm-scalarnew /tmp/eng | grep -c REPLACED          # 0
grep -rh cratonvm-scalarnew /tmp/eng | grep -oE 'refused [A-Za-z]+' | sort | uniq -c

# the positive control
CRATONVM_SCALAR_DEOPT=1 CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_SCALAR_DEOPT=1 \
  cratonvm --java-home <jdk25> -cp probes/voxout \
  -Dvoxel.warm=4 -Dvoxel.warmiters=20000 VoxelAlloc2 2>&1 | grep DBG_SCALAR_DEOPT
```

`netty-nonpassed-latest.txt` is CRLF and `testlist.txt` is LF: subtracting them
without `tr -d '\r'` silently matches nothing and hands you the full 733-class
list as a "pass set".
