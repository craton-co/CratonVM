# `AbstractMethodError: AnnotatedElement.getDeclaredAnnotations() has no Code attribute` — only after ~131 tests in the same JVM

| | |
|---|---|
| **Status** | 🔴 OPEN — real VM defect, reproduced once with a full per-test listener. Not reproducible in isolation. |
| **ID** | `HIB-ANNOTATEDELEMENT-NOCODE.1` |
| **Found** | 2026-07-31, in the first CratonVM run of `DefaultCatalogAndSchemaTest` that reached the end of the class (which only became possible once `HIB-GCOVERHEAD-HALFFULL.1` was fixed). |
| **Not** | a regression from that fix, and not the same defect. The OOM killed the process at ~41 min; this is a single failing test at the ~116-minute mark. |

## Symptom

```
[class-template-invocation:#12]/[method:updateSchema_fromSessionFactory(DomainModelScope)]
java.lang.AbstractMethodError: method java/lang/reflect/AnnotatedElement
    .getDeclaredAnnotations()[Ljava/lang/annotation/Annotation;
    has no Code attribute
```

`java.lang.reflect.AnnotatedElement` is an **interface** and
`getDeclaredAnnotations()` is abstract on it. The call therefore reached the
interface's own declaration rather than the receiver's concrete override —
`Class`, `Method`, `Field` and `Executable` all implement it. Something in the
dispatch chain stopped resolving to the receiver's runtime class.

## Why it is expensive to work on

**It does not reproduce in isolation.** Selecting the exact failing unique id:

```
[engine:junit-jupiter]/[class-template:...DefaultCatalogAndSchemaTest]/
[class-template-invocation:#12]/
[method:updateSchema_fromSessionFactory(org.hibernate.testing.orm.junit.DomainModelScope)]
```

passes on CratonVM **3 runs out of 3** (and on HotSpot). It fails only when the
other 131 tests of the class have run first in the same JVM. That is the
in-class-pollution shape recorded in
`reference_junit_request_method_defeats_inclass_repro`, and it means every
iteration costs a full ~2-hour class run.

## Evidence, and the one thing that contradicts itself

Two full CratonVM runs of the class, same binary, disagree:

| run | runner | result |
|---|---|---|
| A | `CratonRunner` (`-Dcraton.batch=1`) | `found=121 started=121 ok=121 failed=0`; 121 `HHH000490` lines |
| B | `ListingRunner` (logs every container and test as it finishes) | **132 tests**, all 12 invocations × 11 methods, 131 `SUCCESSFUL`, 1 `FAILED` (above) |
| — | HotSpot, either runner | 132, all successful |

Run A looks like eleven tests going missing; run B enumerated all 132. Both
runs are on the same binary and the same classpath. So the residual is
**non-deterministic**, and "eleven invocations are lost" is *not* an established
mechanism — do not start from it. The two differences between the runners worth
eliminating first are `-Dcraton.batch=1` and the listener set
(`SummaryGeneratingListener` vs. a plain `TestExecutionListener`).

Note that `CratonRunner` cannot show a container-level failure: its dump is
gated on `failed != 0 || started != ok + failed + aborted`, and a failed
*container* is not a failed *test*. Run A printed nothing. Any future
investigation should use `ListingRunner`, which reports both.

## Where to start

`vm/src/runtime/interpreter.rs` raises this at the `!has_code` arm of `execute`
(search `has no Code attribute`). It already has a receiver-walk rescue that
re-dispatches on the receiver's runtime class, plus a hard-coded
interface→canonical-class map, and a diagnostic:

```bash
CRATONVM_DBG_NOCODE=1
```

which prints `[DBG_NOCODE] <msg> | recv_cid=<id> recv_class=<name>` at the raise
site. The first question that answers: **is the receiver a real
`Class`/`Method`/`Field`** (so the receiver walk should have found the concrete
override and the bug is in the walk), **or is it something else** (a stale or
corrupted `ObjectRef`, in which case this is a lifetime bug wearing a dispatch
bug's error message)?

Given the ~131-test warm-up needed, that diagnostic run is the cheapest next
step — one full class run, with the answer in the stderr.

## Related

- [`../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md)
  — the OOM whose fix made this class reach its own end for the first time, and
  so made this visible.
- [`moving-young-inert-under-jit-throughput-tax-20260730.md`](moving-young-inert-under-jit-throughput-tax-20260730.md)
  — why the class takes ~105 min where HotSpot takes 120 s. Separate, and the
  reason a single iteration here is so slow.
