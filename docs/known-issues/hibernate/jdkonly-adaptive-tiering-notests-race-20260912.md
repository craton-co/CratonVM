# jdk-only + adaptive tiering: 3 classes silently discover 0 tests — not reproducible when tiering is pinned

## Status
**OPEN, not root-caused.** New finding from a 4-arm hib-reactive comparison,
not in any existing known-issues page.

## Measured
2026-09-12, `dev@9e7f2ed4d`, local Windows box, 247-class
`testlist-noport8088.txt`, real Postgres via Testcontainers, `-Parallel 1`
(serial per arm), four arms from one binary:

| arm | mode | tiering | PASS | FAIL | NOTESTS |
|---|---|---|---:|---:|---:|
| default-plain | stock compat | adaptive (engine default) | 202 | 1 | 44 |
| default-c1 | stock compat | `CRATONVM_C2_SUPERSEDE=0` (pinned single-pass) | 202 | 1 | 44 |
| **jdkonly-plain** | `--jdk-only` | **adaptive (engine default)** | **199** | 1 | **47** |
| jdkonly-c1 | `--jdk-only` | `CRATONVM_C2_SUPERSEDE=0` (pinned single-pass) | 202 | 1 | 44 |

The single FAIL is identical in all 4 arms
(`MultithreadedInsertionWithLazyConnectionTest`, already documented in
`hib-reactive-multithreaded-insertion-lazy-connection-20260822.md` — not new).
The 44 NOTESTS in the other 3 arms are the suite's normal DB-connectivity/
fixture-shape gaps. **`jdkonly-plain` is the only arm of the four with 3 extra
NOTESTS**, and it's the only arm that is both `--jdk-only` AND left at
adaptive tiering — the other three each hold one of those two variables fixed
at "not that."

## The 3 classes, and what makes them NOTESTS only there

```
org.hibernate.reactive.EagerElementCollectionForBasicTypeListTest
org.hibernate.reactive.FilterWithPaginationTest
org.hibernate.reactive.UnionSubclassInheritanceTest
```

All three score `found=0 started=0 ok=0 failed=0` in `jdkonly-plain` — JUnit's
own discovery phase finds zero test methods, not a class-load failure that
surfaces as an exception. In the identical spot in `jdkonly-c1` (same
`--jdk-only` mode, tiering pinned instead of adaptive), all three run their
full test count cleanly: `found=29/35/6`, all OK.

## The only difference in the log: still just noise, in both arms

Both arms print the same `--jdk-only` warning constantly:
```
WARN cratonvm_classloading::class_manager: refusing to fabricate a
compatibility stand-in for this class. It is NOT registered and the
requester above did not get its receiver, so the failure will surface
later, at that caller's own call site and usually naming a different class.
...
class="java/util/Enumeration$Impl" fields=5
requested_by="native-builtins\src\classloader.rs:5639" ... occurrence=N
error=class file error: class not found: java/util/Enumeration$Impl
```
This fires **7696 times across the whole `jdkonly-c1` run alone** (rate-limited
per (class, requester) pair, not per-refusal), including presumably around
these same 3 classes, which still passed there. So the warning by itself is
not the failure — it's ambient background noise under `--jdk-only` mode in
general (`java.util.Enumeration$Impl` is a JDK-internal enumeration
implementation something in the classpath/ServiceLoader/reflection scanning
path touches, and jdk-only mode refuses to fabricate a compatibility
stand-in for it). The warning's own text says exactly what's happening here:
*"the failure will surface later, at that caller's own call site and usually
naming a different class"* — consistent with a deferred failure whose
visible symptom (JUnit discovery silently returning zero results, no
exception bubbling to the harness) depends on some other condition that only
coincides with it under adaptive tiering.

## Working hypothesis, not verified

The variable that changes between `jdkonly-plain` (broken) and `jdkonly-c1`
(clean) is exactly the same one `tier-ab.sh` exists to isolate: whether
methods get promoted from the single-pass tier to the optimizing tier
mid-run. Under adaptive tiering, a background compile thread can fire and
recompile a hot method (e.g. something in the JUnit discovery/reflection
path, or the `Enumeration`-adjacent classloading code itself) *while*
discovery for one of these 3 specific classes is in flight; under
`CRATONVM_C2_SUPERSEDE=0` nothing is ever promoted, so that recompilation
event — and whatever race it exposes — simply never happens. This would make
it a scheduling-timing race gated on background JIT promotion, not a
`--jdk-only`-specific defect in the ordinary sense (it never fires without
`--jdk-only` either, per the 0-occurrence grep against `default-plain`, so
both conditions — `--jdk-only`'s stricter class-fabrication refusal AND live
tier promotion — appear to be required together). **Not traced further**:
nobody has looked at what specifically recompiles around the moment
discovery for these 3 classes runs, or confirmed the race reproduces
standalone (single class, repeated, under `jdkonly` + adaptive tiering only).

## Not yet done
- Reproduce standalone: run just these 3 classes, `--jdk-only`, adaptive
  tiering, repeated several times, to see if this is a consistent 3/3 or a
  flake.
- `CRATONVM_DBG_JIT_METHOD_STATS=1` on a standalone repro, to see what
  actually gets promoted around the failing window and correlate it with
  whatever calls into `Enumeration$Impl`.
- Check whether `--jdk-only-report`/`--explain-jdk-only` on a standalone
  repro of one of these classes shows a *different* down-stream class name
  at the point discovery actually gives up (the warning's own text implies
  the visible failure names a different class than the one really missing).

## Repro
```bash
cd apps/hibernate-reactive-suite-runner
CRATONVM_DBG_JIT_METHOD_STATS=1 ./cratonvm-jdkonly-20260912.sh \
    --java-home "$JDK" --Xmx 1500m @common.args -Dcraton.batch=1 \
    CratonRunner org.hibernate.reactive.EagerElementCollectionForBasicTypeListTest
# repeat several times; compare against the same invocation with
# CRATONVM_C2_SUPERSEDE=0 exported first
```
