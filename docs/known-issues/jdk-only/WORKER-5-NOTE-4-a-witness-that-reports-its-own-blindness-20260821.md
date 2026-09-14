# WORKER-5 NOTE 4 — ten of thirteen witness signals are blind, and the three that are not ask WHICH CODE RAN rather than WHAT IT COMPUTED

**Status: MEASURED.** Lane WORKER-5, 2026-08-21, against
`C:/craton/cratonvm-r10.exe`, oracle HotSpot 25.0.3+9. No VM change.

> **Re-run on `cratonvm-r11.exe` after merging H0's `dab993033`** (which brings
> `ecd4f56e1`, a `java/util/Comparator` guard in `native-collections`). The whole
> table below reproduces **exactly** — 3 of 13 for `put` and `get`, 1 of 14 for
> `iterate`, same signals, same values, same oracle column. A change in the
> collection natives did not move the HashMap witness.

---

## 1. The problem, restated so the fix is legible

`H0-8` §7 records it in one sentence: **a fix to the VM removed a witness.**
`H16-2` taught the native to mint real `HashMap$Node`s, so the bucket-head
witness — which had been reading `AnonymousObject$4` on the native path —
started agreeing in both directions. `H17` then measured **four of six blind on
a current binary**: bucket head, `modCount`, `hashCode()` counts, `equals()`
counts.

The dangerous part is not that they went blind. It is that **a blind witness
does not report blindness — it AGREES, and agreement reads as "no defect".**

And WORKER 1 is blocked on exactly this. `H17-2`: `jdk_only_enforce_shadow_for`
has one live call site, inside `resolve_step1_native`, so arming a class arms
only its cold, step-1 dispatches. An armed FAILURE is real; **an armed ZERO is
unreliable** — and a zero from a blind witness is indistinguishable from a zero
from a fixed VM.

## 2. The design rule this file is built on

> The instrument's job is not to be right about the VM. It is to state, from the
> run itself, WHICH of its signals can currently tell the two dispatch paths
> apart — and to refuse a verdict when none of them can.

`regression-suite/probes/DispatchWitness.java` emits thirteen named signals and
decides nothing. `regression-suite/probes/dispatch-witness.sh` runs it unarmed
and armed, ONE CASE PER PROCESS (`H0-8`), REPEAT times per arm, and labels every
signal:

| state | meaning |
|---|---|
| `LIVE` | the arms differ and each arm agrees with itself |
| `BLIND` | the arms agree |
| `UNSTABLE` | an arm disagrees with ITSELF across repeats — never counted LIVE |
| `MISSING` | a signal one or both arms did not emit — never confused with BLIND |

`UNSTABLE` and `MISSING` are not decoration. Without `UNSTABLE`, noise reads as
discrimination; without `MISSING`, a signal that stopped being emitted reads as
agreement. Both are the same mistake in the other direction.

## 3. MEASURED

`scope=java/util/HashMap`, `REPEAT=2`, one case per process. The oracle column
does not decide liveness — it says WHICH ARM IS RIGHT, which is the other
question.

### case = put — 3 of 13 discriminate

| | signal | unarmed | armed | oracle |
|---|---|---|---|---|
| **LIVE** | `frames` | `none` | `java.util.HashMap.hash,java.util.HashMap.put` | = ARMED |
| **LIVE** | `framedepth` | 3 | 5 | 5 |
| **LIVE** | `iter` | `HashMap$EntryIterator` | `throws:NullPointerException` | = UNARMED |
| BLIND | `case` `op` `hccount` `eqcount` `table` `head` `tablen` `modcount` `consistent` `sizes` | | | |

### case = get — 3 of 13

Same three. `frames` is `hash,getNode,get`; `framedepth` 3 → 6.

### case = iterate — 1 of 14

| | signal | unarmed | armed | oracle |
|---|---|---|---|---|
| **LIVE** | `iterated` | 8 | **1** | 8 |

That is `H0-4` §7's *"`keySet()` yields 1 of 3000"* reproduced at n=8.

## 4. What the table says

**Ten of thirteen signals are blind. All three survivors are dispatch-side or
defect-side.** That is the reusable sentence:

> A witness that asks WHAT WAS COMPUTED dies when the VM computes the right
> thing. A witness that asks WHICH CODE RAN does not, because "more correct"
> never means "reproduce the JDK's internal frame names".

The live one is a key whose `hashCode()` captures
`Thread.currentThread().getStackTrace()`. `hashCode()` is the one JDK-internal
call site an application can legally stand inside, which is what makes the
frame list reachable at all. `H17` had already measured hashCode COUNTS blind —
and they are, here, at 8 on both arms. **Counting how often it ran is
value-side; capturing where it ran FROM is not.** Both are in the table so the
distinction is visible instead of being re-derived.

`head` is deliberately still measured and prints `BLIND`. It is the witness
`H16-2` killed, and printing it blind is the whole point of the exercise.

### 4.1 Two findings that fell out

* **`iter` under arming throws an NPE from `HashMap$EntryIterator.<init>`**, and
  the ORACLE agrees with the UNARMED arm — so this is a defect on the armed
  path, not a native/bytecode difference of opinion. It is the `H0-5` mechanism
  A shape (a view carrier with a null `this$0`) reached through real bytecode.
* **It killed the first version of the probe.** The unguarded call sat after the
  frame signals and before `iter`, so the process died mid-output and the driver
  would have scored `iter` as MISSING — i.e. the defect would have hidden itself
  by truncating the evidence. Every signal is individually guarded now, and a
  throw is published as a VALUE (`throws:java.lang.NullPointerException`) rather
  than as an absence. **A witness must survive the VM being wrong, not only the
  VM being right.**

## 5. The failure path, EXERCISED

A gate that cannot fail is worse than no gate. Arming a class the probe never
touches:

```
$ dispatch-witness.sh put java/nio/file/NoSuchThing
  0 signal(s) DISCRIMINATE, 13 blind, of 13.
  THE INSTRUMENT IS BLIND. Every signal agrees across the two arms, so this
  run cannot tell a fixed VM from an unobservable one. Do NOT read a zero
  from it as evidence of anything — add a signal before measuring again.
  rc=1
```

MEASURED, not argued. `--selftest` additionally drives the scorer with no VM and
no JDK over LIVE, BLIND, UNSTABLE, MISSING (both one-sided and never-emitted),
the all-blind refusal, and output ordering that does not depend on input
ordering.

## 6. How to use it

```bash
JDK=$(cygpath -m "$(dirname "$(dirname "$(command -v javap)")")")   # trap 1
CV=/c/craton/cratonvm.exe JDK="$JDK" \
  regression-suite/probes/dispatch-witness.sh put java/util/HashMap
```

Before quoting an armed zero for any class, run this for that class. If it says
the instrument is blind, the zero means nothing yet.

## 7. What this does NOT establish

* **It is a HashMap probe.** The three cases are `put`, `get`, `iterate` on
  `java.util.HashMap`. `SCOPE` is a parameter, but the OPERATIONS are not — a
  different class needs a different case body. Nothing here generalises to
  `ConcurrentHashMap`, `ArrayList` or the collection cluster without new cases.
* **`frames` is live TODAY.** A VM that grew a stack-walk shim naming JDK frames
  on the native path would blind it, and the honest response to that is that the
  harness would then print `frames BLIND` — which is the property being bought,
  not immunity.
* **Liveness is measured between UNARMED and ARMED, not between NATIVE and
  BYTECODE.** The dial reaches one of four+ dispatch doors (`H17-2`), so a
  signal that is BLIND here may still discriminate at a door the dial cannot
  reach. `BLIND` means "this arm pair cannot see it", not "no instrument can".
* **Nothing was run under `--nojit` or `CRATONVM_DISABLE_INTRINSICS=1`.** `G33-1`
  says a census whose `invocations` column is exact needs both; this witness
  does not read invocation counts at all, so the caveat does not apply to its
  numbers — but a lane combining the two must not assume they were taken under
  the same conditions.
* **`REPEAT=2` is two, not a distribution.** `UNSTABLE` catches a signal that
  moved between two runs. A signal that moves one time in ten reads stable here.

## 8. NOMINATIONS

* **N1 — WORKER 1 should run this before and after its fix**, per class it
  arms. `frames`/`framedepth` are the signals that will move; `head`, `table`
  and `modcount` will not, and that is not evidence.
* **N2 — the `iter` NPE under arming is a live defect** (§4.1) and is not this
  lane's file. It reproduces in nine lines and the oracle disagrees with the
  armed arm.
* **N3 — a `frames`-style witness generalises**, and cheaply: any operation that
  calls back into application code (`hashCode`, `equals`, `compareTo`,
  `toString`, a `Comparator`, a `Function` passed to `computeIfAbsent`) can
  capture its own caller frames. That is a bigger surface than the six witnesses
  this directory has been using, and none of them was of this kind.
* **N4 — the six historical witnesses should be retired or re-labelled.** Four
  were measured blind by `H17`; this record measures ten of thirteen blind on a
  wider set. A record quoting one of them as evidence should say which binary it
  was live on.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-4` — a witness that reports its own blindness.
  `regression-suite/probes/DispatchWitness.java` +
  `regression-suite/probes/dispatch-witness.sh`. **10 of 13 signals BLIND on
  `java/util/HashMap`; the 3 live ones are dispatch-side** (caller frames
  captured inside `hashCode()`) **or defect-side.** The all-blind refusal is
  exercised (rc=1). Also: `entrySet().iterator()` throws NPE under arming, and
  the oracle sides with the unarmed arm. MEASURED.
