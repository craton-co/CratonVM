# bug-04: interned String constant corrupted to bare `java.lang.Object` under batched load

| | |
|---|---|
| **Category** | **VM-CORRECTNESS / GC** (load-dependent; ⊆ the GC-root-undercount race) |
| **Affected** | any String constant live across heavy batched execution (observed: KRun's `"OK"`/`"FAIL"` literals) |
| **CratonVM** | a `String` literal reads back as a `java.lang.Object` instance (`toString()` = `java.lang.Object@<hash>`) |
| **HotSpot JDK 25** | n/a (string identity stable) |
| **CratonVM HEAD** | `8e8e47d9` (suite run) |
| **Status** | 🟡 PARTIAL (audit 2026-06-19) — Family-A manifestation of [[spring-bug-10-junit-platform-execution-loaderr]]; the cure (precise shadow-stack roots) exists **only behind `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1`** (default-OFF). Under **default** flags this still reproduces (a `String` literal reads back as `Object`); NOT a crash here |
| **Suggested owner** | handoff / GC-focused (architectural, deferred) |

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
