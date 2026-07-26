# SPB.1 (`org/springframework/util/`) re-investigation — inconclusive, but surfaced a separate real crash

**Status:** Ban KEPT (insufficient evidence to remove). A separate,
possibly serious heap-corruption crash was found and needs its own
follow-up, but is NOT cleanly attributable to this specific ban.

## Context

Priority item 2 from `docs/known-issues/jit-skip-list-open-bans-20260725.md`'s
"Recommended next session priority": re-test whether the SPB.1
"allocate-then-putfield" theory (`org/springframework/util/`, specifically
`ClassUtils.registerCommonClasses`'s ~100-`HashMap.put` loop) still holds
post the 2026-07-04 general fix, or whether — like
TOMCAT-DOHEAD-JUNIT-ITERATOR.1 — it might be stale. No fixture app
(`apps/SportMe-master`) is available on this host, so built three
progressively more faithful standalone repros against a real
`spring-core-7.0.7.jar` (found in `~/.gradle/caches/...`, not in `~/.m2`).

## Repro 1 — minimal (`ClassUtilsProbe.java`)

`Class.forName("org.springframework.util.ClassUtils")` once, then read back
`commonClassCache` via reflection and exercise `ClassUtils.forName` 3.4M
times (200k iterations × 17 primitive/array type names). **Both baseline
(ban in place) and lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/util/`)
passed cleanly, 0 errors, 0 mismatches.**

## Repro 2 — HashMap-warmed (`ClassUtilsProbe2.java`)

Same, but runs 500×200 `HashMap.put` calls first to force
`put`/`putVal`/`newNode`/`afterNodeInsertion` JIT-hot *before* triggering
`ClassUtils.<clinit>` — closer to a real Spring Boot boot where those
methods are already hot by the time `ClassUtils` loads (matches the
original ban comment's own crash-frame sequence). **Both configs still
passed cleanly.**

## Repro 3 — GC-pressure + classloader churn (`ClassUtilsProbe3.java`)

Added the other ingredient the ban comment calls out ("esp. across a
GC-triggering call"): a concurrent daemon thread continuously allocating
64KB arrays and calling `System.gc()`, while the main thread re-triggers
`ClassUtils.<clinit>` 30 times via a fresh `URLClassLoader` per round (since
clinit only runs once per defining loader). **Both configs crashed — but
differently, and neither the way the original bug was described:**

- **Baseline (ban in place):** `cratonvm::gc::guard: gen_heap::read_slot:
  corrupt Value cell (out-of-range discriminant)` — 15 corrupted-slot errors
  logged, then `ClassUtils.<clinit>` fails with
  `NullPointerException: Cannot invoke "Class.getName()" because "clazz" is
  null` inside `registerCommonClasses` (line 202) — this **is** the original
  bug's exact crash site/shape, but under the *baseline* config where
  `org/springframework/util/` should be running fully interpreted (the ban
  should prevent any JIT miscompile in this package specifically).
- **Lifted:** `ClassCastException: org.springframework.util.ConcurrentReferenceHashMap$Reference
  cannot be cast to java.util.Map` — reading back `commonClassCache`
  returns an object of the wrong type (a `ConcurrentReferenceHashMap$Reference`,
  used by an unrelated internal cache elsewhere in the same class) instead
  of the `Map` it should be. This looks like classic field-slot corruption
  (right shape, wrong content) but is a different symptom than baseline's
  crash.

## Why this is inconclusive for SPB.1 specifically

**Baseline crashing at all is the confusing part.** If the ban fully
protects `org/springframework/util/*` from JIT compilation, a crash whose
own stack trace lands inside `registerCommonClasses` under baseline
shouldn't happen from a JIT miscompile *in that package* — unless:
(a) the real corruption source is elsewhere (e.g. in JIT-compiled
`URLClassLoader`/GC-root-scanning code exercised by the aggressive
loader-churn-under-GC-pressure pattern this repro uses, which is NOT
covered by the SPB.1 ban at all), and `ClassUtils.<clinit>` is just the
*first* place a corrupted heap slot happens to get read back — i.e. this
repro's own design (rapid loader creation/close + concurrent forced GC) may
be triggering a **different, more fundamental bug** than the one SPB.1
targets, or
(b) there's a race/timing element (the GC-pressure thread is inherently
non-deterministic) that makes this an unreliable differential test as
constructed.

Both are plausible; distinguishing them needs more work (e.g. rerun several
times to check determinism/flakiness, or strip the loader-churn to isolate
whether GC-pressure alone vs. loader-churn alone is the trigger — not done
this session due to time).

## Disposition

**KEEP `org/springframework/util/` banned** — no positive evidence found to
justify removing it (repros 1 and 2 are clean but likely not faithful
enough to the original trigger; repro 3 is a real crash but not cleanly
attributable to *this* ban specifically, and crashes with the ban ACTIVE
too, which argues against "lifting this ban causes the regression" as a
clean conclusion).

**Separately worth its own investigation** (not undertaken further this
session): the repro-3 crash pattern itself — rapid `URLClassLoader`
creation/`close()` under concurrent GC pressure producing
`gen_heap::read_slot: corrupt Value cell` heap-integrity errors — happens
**regardless of this specific JIT ban** and could be a genuine, more
fundamental GC-root/class-unloading defect worth someone picking up
independently. Repro: `docs/known-issues/repros/spb1-classutils/ClassUtilsProbe3.java`
(needs a real `spring-core-*.jar` on classpath at compile time only, plus
`-Dspring.core.jar=<path>` at runtime; probes 1 and 2 are also included in
that directory for reference).

## Established pattern across this session's three SPB/CGL/PIC-family tests

| Ban | Test method | Result |
|---|---|---|
| `org/jboss/as/` | Real WildFly boot | Confirmed live JIT-only bug (`ModelTypeValidator.validTypes` NPE) — KEEP |
| `org/h2/` | Real 218-class H2 suite | Confirmed live correctness bug (`Schema not found` on reconnect) — KEEP |
| `org/springframework/util/` | Synthetic standalone repros (3 iterations) | Inconclusive — KEEP (no positive evidence to remove) |

Two real-app tests found live bugs; the one synthetic-only test (no real
fixture app available) found nothing conclusive either way. This reinforces
a general lesson for whoever continues this sweep: **prefer testing against
a real app/suite over a hand-rolled synthetic repro when one is available**
— synthetic repros for this specific "allocate-then-putfield" bug family
have so far proven much harder to construct faithfully than the ban
comments' own descriptions suggest.

## Related

- `docs/internal/jit-ban-sweep-20260725.md` — this session's tracking doc.
- `docs/known-issues/jit-skip-list-open-bans-20260725.md` — the shared
  cross-session coordination doc this priority item came from.
