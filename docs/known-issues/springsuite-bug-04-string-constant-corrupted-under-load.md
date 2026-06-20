# bug-04: interned String constant corrupted to bare `java.lang.Object` under batched load

| | |
|---|---|
| **Category** | **VM-CORRECTNESS / GC** (load-dependent; ⊆ the GC-root-undercount race) |
| **Affected** | any String constant live across heavy batched execution (observed: KRun's `"OK"`/`"FAIL"` literals) |
| **CratonVM** | a `String` literal reads back as a `java.lang.Object` instance (`toString()` = `java.lang.Object@<hash>`) |
| **HotSpot JDK 25** | n/a (string identity stable) |
| **CratonVM HEAD** | `8e8e47d9` (original suite run) |
| **Status** | 🟡 **OPEN / needs batch re-verify** (2026-06-20). **Not observed** on dev `697134f8` in the 2026-06-20 per-class + small-batch runs (status fields are clean `OK`/`FAIL`, no `java.lang.Object@…`). Family-A precise-maps are default-on now, and the `ReferencePipeline.toArray(IntFunction)` self-recursion that destabilised the JUnit launcher under load is fixed (`8795b88d`). A large single-JVM batch is still needed to confirm it no longer reproduces before this can be archived. |
| **Suggested owner** | GC-focused; confirm via a big batch, then archive |

> **2026-06-20 note.** The original repro relied on the old `KRun` harness corrupting its own
> `"OK"`/`"FAIL"` literals across a heavy batch JVM on binary `8e8e47d9`. On dev `697134f8`
> (build `cratonvm-spring0620`) the recreated `KRun` reports clean `status=` fields across all
> 2026-06-20 runs to date. Because the corruption is load-dependent it must be re-checked with a
> large single-JVM batch (the per-class runs below do **not** build the required GC pressure).

## Symptom
In the batched suite run, ~6 classes recorded a `status` field of `java.lang.Object@<hash>` instead of
`OK`/`FAIL` in `results.tsv`. KRun computes `status` as a String literal
(`(fail==0) ? "OK" : "FAIL"`), so the literal itself was replaced by a bare `java.lang.Object`.
Only **two** distinct hashes recur — one for all-pass classes (the `"OK"` constant) and one for
has-failure classes (the `"FAIL"` constant) — i.e. the *interned constants* were corrupted
process-wide within a batch JVM, not per-call.

```
org.springframework.validation.ValidatorTests   java.lang.Object@54eee1d2  found=2 succ=2 fail=0   # "OK" corrupted
org.springframework.validation.DataBinderFieldAccessTests  java.lang.Object@5433ceda  found=7 succ=5 fail=2  # "FAIL"
... (BshScriptEvaluatorTests, ModelExtensionsTests, RefreshableScriptTargetSourceTests, TaskExecutionOutcomeTests)
```

## Not reproducible in isolation
Running the same classes **one per JVM** yields the correct `status=OK`/`FAIL`:
```
RESULT org.springframework.validation.ValidatorTests ... status=OK
RESULT org.springframework.ui.ModelExtensionsTests ... status=OK
```
So the corruption requires the accumulated GC pressure of a **batch** JVM (many classes + JUnit
threads), exactly the profile of the documented young-gen root-undercount race.

## Root cause (suspected) + relation
Same family as [[spring-bug-10-junit-platform-execution-loaderr]] and memory
`jit-junit-discovery-reflection-corruption`: under heavy multithreaded JUnit execution the young
collector treats a live object as dead (root invisible on a parked worker / shadow-stack register
gap) and the slot is reused → a `String` constant's storage comes back as a different object
(`java.lang.Object`). Here it hit interned String constants; elsewhere in the family it zeroes
engine/listener/enum objects (→ LOADERR / NPE / CCE / occasional CRASH). Architectural GC fix;
deferred.

## Impact on this run's tallies
Cosmetic for accounting only — the `found/succ/fail` counts on those rows are intact, so their real
status is re-derived from counts (`fail==0 ⇒ OK`, else `FAIL`). The whole-suite status totals in
[INDEX](INDEX.md) apply that reclassification. No test was lost.

## Notes
- A non-crashing *symptom* of a crash-capable race — worth fixing for the same reason as
  spring-bug-10, but out of this run's direct crash/hang scope. Good GC-focused handoff.
