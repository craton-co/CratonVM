# The OSR door refused every exception-table callee unconditionally, and reported neither its binds nor its refusals — and the probe written to close that found two wrong answers

**Status: FIXED 2026-08-24. Both halves of
`static-exception-table-callee-pays-the-funnel-20260821.md` are closed.** The
static-door ban was lifted on 2026-08-21 (10.2x per call, measured then); the
OSR half — the whole of that page's remaining content — is closed here, at
**14.5x** on the same probe, with the OSR door now visible in the census. Two
correctness defects that the closing work exposed are fixed with it, and both
were live on the DEFAULT configuration of `dev`.

Reproducers: `probes/NativeFunnelFloorProbe.java` (the cost),
`probes/ExcTableDirectCallOracle.java` (the callee's own handler, now with two
OSR arms), `probes/EscapeStaticProbe.java` and `probes/EscapeKindProbe.java`
(the caller's handler — new, and the two defects below are theirs).

---

## 1. What the previous page established, and where it stopped

`jit::direct_call_exc_table_publish_enabled` gates whether a STATICALLY BOUND
call site may bake a direct machine-code `CALL` to a callee that declares its
own exception table. The ban was measured at **10.2x per call** on
`NativeFunnelFloorProbe`'s ordinary-frame rung (107.30 → 10.57 ns/op, both
controls flat) and lifted default-ON on 2026-08-21 — on a consistency argument,
not a workload win, and that page says so rather than being rewritten to agree
with the decision. That verdict stands and is not revisited here.

What it could not close was its second finding:

> **An OSR body never consults the gate at all.** The second row does not move
> when the switch is flipped … `direct callee binds: 0 bound, 0 left on the
> dispatch helper` … in **both** arms. Zero sites examined, zero refusals
> tallied.

and its remaining step:

> 3. **Give OSR bodies the direct-bind ladder**, or record why they cannot have
>    it.

## 2. Half right, and the wrong half was the instrument

The OSR door does have its own direct-bind ladder. `compile_osr_artifact`
reaches `x64::compile_with_param_slots` directly and carries a hand-written
copy of the binding rules — it binds `invokestatic` and `invokespecial`
callees, it binds the String and Math intrinsics, it binds
`Thread.currentThread`. It was binding and refusing all along.

Two things hid that:

* **`osr_callee_declares_handlers`** returned "refuse" for ANY callee declaring
  an exception table, **unconditionally**, while the other two doors had asked
  `direct_call_exc_table_publish_enabled` since that gate existed and stopped
  refusing when it flipped. So the door did consult a policy — its own, three
  days stale.
* **the ladder tallied nothing.** `DIRECT_CALLEE_BIND_HITS`/`MISSES` and the
  refusal table were bumped by the other two doors only. A door that reports
  nothing and a door that does nothing print the same census line, and the
  census line was the evidence the previous page reasoned from.

A hot loop is always an OSR body. So the sites that stand to gain most from a
direct call were, by construction, the ones systematically excluded from the
population the switch could act on — which is exactly the coupling that page
suspected and could not confirm.

## 3. The fix

* **`osr_callee_bars_direct_call`** replaces the predicate and asks the same
  gate the other two doors ask, returning the REASON rather than a bool. The
  refusals that are not about the gate stay unconditional: a callee that cannot
  be resolved, and one with no `Code` attribute at all — there is no body to
  bake a `CALL` to. (`direct_callee_lookup` maps the no-`Code` case onto
  `CalleeExceptionTable` too; this mirrors it so the census table means the same
  thing at all three doors.)
* **the ladder tallies**, into `DIRECT_CALLEE_BIND_HITS`/`MISSES` and the shared
  refusal table, plus an OSR-only subset pair printed as
  `of which the OSR door: N bound, M left`. A zero on that line is now a real
  statement about a door that ran.

Nothing else had to change for this to be sound. The gate's two stated
preconditions are properties of the EMITTER, not of the door:
`emit_inline_callee_deopt_check` is emitted after the baked `CALL` by the same
`x64/bytecode_walk.rs` arms this door reaches through
`compile_with_param_slots`; a site that cannot reserve the service slots fails
the compile (`direct-call-service-slots`) rather than emitting an unserviced
edge; and the OSR ladder already registers a `JitInvokeInfo` for every
direct-bound site, which is what that check reads.

## 4. The measurement — 14.5x, and the rung finally moves

`probes/NativeFunnelFloorProbe.java`, ABBA on ONE binary
(`cratonvm-osrbind`), 3 rounds, n=2 000 000, `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH`
1 vs 0, ns/op. Azure host, 1-minute load 11–19 (recorded because the absolute
numbers are inflated by it; the ratio is same-process and same-round):

| rung | ban lifted (default) | ban kept | |
|---|---:|---:|---|
| **static call, callee has `try`/`catch`, from an OSR loop** | **9.27–12.49** | **140.68–173.51** | **~14.5x** |
| the same callee via one ordinary frame (`hop`) | 13.60–19.30 | 150.79–165.17 | ~10x |
| *control* plain static call, no table | 9.32–10.78 | 9.37–10.53 | flat |
| *control* interface call, callee has `try`/`catch` | 14.49–20.02 | 14.51–20.63 | flat |

`sha256(0..7)` byte-identical in all twelve runs. On the previous page the OSR
rung read **102.68 → 100.26, unmoved**; it now moves with the gate, and it is
faster than the via-hop rung, which is what it should be — the hop is a frame.

One outlier is recorded rather than dropped: round 1's ban-kept via-hop rung
read 1443.95 ns/op against 150–165 in every other ban-kept round. The host was
shared and loaded; the ON/OFF pairs either side of it are consistent.

### 4.1 The census now names the door

`CRATONVM_DBG=intrinsic-stats`, same probe, n=600 000:

| | lifted (default) | ban kept |
|---|---|---|
| `direct callee binds` | 5 bound, 2 left | 2 bound, 5 left |
| **`of which the OSR door`** | **3 bound, 2 left** | **2 bound, 3 left** |
| `bind refused, callee-exception-table` | absent | **3** |

Both counters move, and the refusal reason appears only in the ban arm. That is
the engagement statement the previous page asked for and could not get.

## 5. The oracle could not have caught what came next

`probes/ExcTableDirectCallOracle.java` pins the CALLEE's own handler — its
`catch`, its `finally`, a wrong-type handler that must not catch, a nested
catch, a rethrow — including the implicit AIOOBE / NPE / div-by-zero cases. It
passes in every arm, on dev and here, and always did.

It pins the wrong direction. The ban's stated reason was *"a raw `CALL` has no
Rust frame to notice the `i64::MIN` sentinel and run the callee's own
handler"*, and the case that actually exercises the sentinel crossing a baked
`CALL` is an exception that is **not** caught in the callee and has to reach the
CALLER's `catch`. No probe covered that. Two did not exist until this work, and
each found a wrong answer on pristine `dev` at its DEFAULT settings.

Two OSR arms were added to the oracle at the same time (`osrSelfCatch`,
`osrPropagate`), because a correctness pin that cannot reach the newly bound
sites is not a pin for them. Output is byte-identical to HotSpot 25 including
those arms.

### 5.1 DEFECT 1 — a `/ by zero` never reached the callee's own exception table

`probes/EscapeStaticProbe.java`: a compiled caller with
`try { guardedWrongType(i); } catch (ArithmeticException e)`, whose callee's own
handler is the wrong type so the trap must propagate out. The driver COUNTS
escapes instead of dying on the first, and records the index of the first one.

On pristine `dev` (`3a4dc5626`), default settings:

```
n=200000 escapes=198000 first=2000 last=199999   CALLER-HANDLER-LOST
```

`first=2000` is the caller's tiering threshold, and the escapes run to the end
of the loop: from the moment the caller is compiled, its `catch` never runs
again. HotSpot and `--nojit` catch all 200 000. It is not a race and not a
window.

**Cause.** `route_implicit_exc_through_callee` — the door that runs a compiled
callee's own `catch` when the callee returned the sentinel — took the AIOOBE
payload and the NPE flag, and had **no arm for the arithmetic flag**. `/ by
zero` is a flag (`stash_jit_pending_arithmetic`), not a stashed throwable, so it
fell into the "general pending exception" branch, found nothing there either,
and left through a bare `return rc`: the callee's exception table never
consulted, the sentinel handed to the next frame out. `CRATONVM_DBG_RBC6=1`
prints `route_implicit_exc_through_callee ENTER … pending_exc=false
has_last_deopt=false` exactly 198 000 times — this function being handed a
signal it had no arm for.

The sibling door for the same sentinel,
`handle_compiled_callee_deopt_sentinel`, has had the arithmetic arm all along.
The two doors simply disagreed, and only one of them is on the helper-dispatch
path.

**Engagement, not masking.** `probes/HelperTrapProbe.java` across three
binaries and three configurations, 3 runs each:

| binary | config | ordinary | bare | deep | osr | explicit |
|---|---|---|---|---|---|---|
| dev | default | **0/3** | **0/3** | 3/3 | 3/3 | 3/3 |
| dev | `DIRECT_EXC_TABLE_PUBLISH=0` | 3/3 | **0/3** | **1/3** | 3/3 | 3/3 |
| dev | `DIRECT_CALLEE_CALLS=0` | 3/3 | 3/3 | **0/3** | 3/3 | 3/3 |
| + OSR bind only | default | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| + OSR bind only | `DIRECT_EXC_TABLE_PUBLISH=0` | 3/3 | **0/3** | **0/3** | 3/3 | 3/3 |
| + OSR bind only | `DIRECT_CALLEE_CALLS=0` | 3/3 | 3/3 | **1/3** | 3/3 | 3/3 |
| **+ arithmetic arm** | all three | 3/3 | 3/3 | 3/3 | 3/3 | 3/3 |

Read the middle block first. The OSR-bind change alone makes the DEFAULT
configuration clean — and it is masking, not fixing: every knob that takes the
direct bind away puts the wrong answer back, in a different arm each time. The
arithmetic arm is what makes every arm clean in every configuration. A fix
whose evidence is one green column is a fix that has not been distinguished
from a re-routing.

### 5.2 DEFECT 2 — a read-only method that traps unresumably died with `InternalError`

`probes/EscapeKindProbe.java` is the multi-call-kind version. Its `virtual`,
`iface` and `special` arms are the same body reached through the dispatch
helper, and on pristine `dev` all three DIE:

```
java.lang.InternalError: JIT dispatch into EscapeKindProbe$Impl.applyVirtual(I)I failed:
  precise deoptimization unavailable for … at bci 5
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "EscapeKindProbe$Impl.applyVirtual:(I)I", reason TransferToInterpreter);
  refusing side-effecting replay
```

where HotSpot and `--nojit` deliver the ArithmeticException to the caller's
`catch`. Only `CRATONVM_DISABLE_JIT=1` cleared it; the tier knobs
(`C2_SUPERSEDE`, `IR_UNRESUMABLE_TRAP_GUARD`, `DEOPT_REAL`, `SCALAR_DEOPT`), the
two exception-table gates and `DISPATCH_CACHE_DIRECT_ENTRY` all left it in
place. The eager callee compile passes `optimize=true` unconditionally, so the
callee goes through the IR pipeline whatever the tier knobs say.

**Cause: two halves of one rule, and only one of them was written down.**
`ir_unresumable_protected_trap` decides whether the optimizing tier may compile
a method whose protected range carries a trap that cannot resume precisely. Its
second narrowing term is stated as:

> only when the range also commits a side effect. A read-only
> `try { return a[i]; } catch (...)` replays harmlessly, so the refusal would
> buy nothing and cost the compile.

The consumer does not replay harmlessly. The interpreter's first-call tier-up
sink refuses EVERY whole-method replay once `can_deopt_resume` is false — side
effects or not — and raises the `InternalError`. So the population the compiler
deliberately let through is exactly the population that dies, and the narrowing
term rested on a behaviour that did not exist.

**Fix: the missing half.** When the method body commits no side effect a re-run
from entry would duplicate, replay it instead of raising. Locals are rebuilt
from the same arguments, nothing outside the frame was written, no call was
made, so the replay is observably the abandoned attempt. Methods that DO commit
side effects keep the refusal unchanged. Both ends now ask one exported
predicate (`opcode_commits_side_effect` / `bytecode_commits_side_effect`) rather
than each carrying its own copy, which is what let them disagree; the walk
answers "side effect" for any instruction stream it loses sync on, so an
unreadable body keeps the old refusal.

After the fix all five arms of `EscapeKindProbe` match HotSpot exactly:

```
static  notable  special  virtual  iface   -> escapes=0, total=1400000, SUM-OK
```

## 6. Validation

* `probes/NativeFunnelFloorProbe.java` — 12-run ABBA, §4, `sha256(0..7)`
  identical in every arm.
* `probes/ExcTableDirectCallOracle.java` — byte-identical output under HotSpot
  25 and under the new default, including the two new OSR arms.
* `probes/EscapeStaticProbe.java` — 5/5 clean where dev is 2/3 broken.
* `probes/EscapeKindProbe.java` — all five call kinds match HotSpot; three of
  them died on dev.
* `probes/HelperTrapProbe.java` — the 3x3x5 matrix in §5.1, clean in every cell.
* `regression-suite/run.sh` — **71 passed, 0 failed** on the fixed binary.
* `cargo test -p cratonvm-jit` — green; the suite total is unchanged because
  the new assertions went inside the existing
  `ir_declines_an_unresumable_protected_trap_and_only_that`, which now pins
  both ends of the rule §5.2 found split.

## 7. What this is worth, and what it is not

The 14.5x is per-CALL on a callee of this shape from an OSR body. It is not a
workload number, and the previous page's census — 3–228 such sites per netty
class against 117–1 252 left on the helper, `native-shadow` dominating — is
still the reason not to expect one. What changes is that the population the
switch can act on is no longer restricted to the colder half, so a future
workload census of this gate is now asking a question the answer can be found
in.

The two correctness fixes are the larger result and they are not about
throughput at all. Both were reachable on the default configuration, both
produce a wrong answer rather than a crash in the common case, and neither had
a probe until the OSR work needed one.

## 8. The lesson worth carrying

A door that reports nothing and a door that does nothing print the same line.
The previous page read `0 bound, 0 left` in both arms of a gate flip and
concluded the gate was unreachable from an OSR body; the conclusion was right
and the instrument was not, and the difference mattered — the ladder was there,
it was making decisions, and its decisions were three days out of date. When an
engagement counter reads zero, establish that the counter is wired before
reading the zero as an answer.

And a correctness pin proves the direction it was written for. The oracle
covered the callee's own handler exhaustively and passed on a tree where the
caller's handler was being skipped 198 000 times out of 200 000.

## 9. Related

* `static-exception-table-callee-pays-the-funnel-20260821.md` — the page this
  closes (the 10.2x, the census, the flip).
* `internal/retired/ir-unresumable-trap-refusal-cost-ANSWERED-20260821.md` —
  where the 12.8x was first seen and mis-attributed.
* `jit::direct_call_exc_table_publish_enabled` — the gate.
* `vm/src/jit/helpers.rs::mic_publish_exception_table_callees` — the sibling
  virtual door.
* `jit::opcode_commits_side_effect` — the predicate §5.2 made shared.
