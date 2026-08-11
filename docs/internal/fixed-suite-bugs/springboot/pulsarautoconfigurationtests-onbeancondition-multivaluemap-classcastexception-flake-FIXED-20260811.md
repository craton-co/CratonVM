---
name: pulsarautoconfigurationtests-onbeancondition-multivaluemap-classcastexception-flake-FIXED-20260811
description: CLOSED 2026-08-11. Both symptoms on this class are resolved. The ClassCastException was fixed 2026-08-07 (Stream.collect holding unpinned references across a moving collection). The "deterministic early HANG" that kept the page open was never a hang - its two pieces of evidence, a 0-byte .out.log and an identical last .err.log line in all three runs, are what EVERY run of this class looks like, pass or fail. Re-measured on 2026-08-11 the class completes 74/74 in 168-379s depending on host load, on the pre-fix binary too. What is real is a throughput ratio against HotSpot, and the class now carries a measured per-class budget in the suite runner.
metadata:
  type: fixed-suite-bug
  area: springboot, pulsar, throughput, suite-harness
---

# `PulsarAutoConfigurationTests` — closed, both symptoms

**CLOSED 2026-08-11.** Filed 2026-08-06 as
`known-issues/springboot/pulsarautoconfigurationtests-onbeancondition-multivaluemap-classcastexception-flake-20260806.md`.

Two symptoms lived on this page.

* The **`ClassCastException: Object cannot be cast to MultiValueMap`** inside
  `OnBeanCondition$Spec` was fixed 2026-08-07:
  `collect_via_collector_protocol`'s ordinary-`Collector` arm held the
  accumulated container, the collector, the accumulator, the finisher and
  every element as raw `ObjectRef`s across five interpreter re-entries with no
  `pin_native_root`, while the same function's other arm pinned all of them.
  Write-up:
  `pulsar-onbeancondition-multivaluemap-stream-collect-pin-FIXED-20260807`
  (retired).

* The **"deterministic early HANG, GC-backend-independent"** added on
  2026-08-07 is what kept the page open. It is closed here, and not by a fix
  — by refuting its evidence.

## The hang was the class finishing normally, off the end of a 300s shard

The page's case rested on two observations, both of which it read as a stall
signature. Neither is one.

**"Zero JUnit output at all — the `.out.log` is 0 bytes in all three runs, not
even the JUnit Platform launcher's banner."** `SbRunner` registers a
`SummaryGeneratingListener` and calls `summary.printTo(pw)` *after*
`launcher.execute(req)` returns. It prints **nothing at all until the run
finishes**, and this class — unlike the Tomcat/Jetty classes triaged in the
same batch — has no embedded server logging to stdout of its own. A 0-byte
`.out.log` is therefore the expected state of a killed run of this class at
any point before the last second, and carries no information about progress.

Measured directly: a clean 74/74 run on 2026-08-11 took 178.6 s, and its
`.out.log` was **0 bytes at t = 90 s and still 0 bytes at t = 177 s**, filling
in the final second.

**"The identical last `.err.log` line, byte-for-byte, in all three runs …
this is a deterministic stall point, not a random one."** The line is a
ByteBuddy code-buffer bail:

```
WARN cratonvm_jit::x64::driver: JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="net/bytebuddy/…/Argument$Binder.bind:(…)…" code_len=244 capacity=62336 wanted=68331
```

It is the last line of **every** run of this class, including the ones that
pass. Three passing runs on 2026-08-11 — on the current binary and on the
pre-fix `cratonvm-default-20260808f.exe` alike — each end with that same
warning (`code_len=244 capacity=62336`, `wanted` differing only by build)
immediately followed by `System.exit(0) called`. Nothing in this class logs
after JIT warm-up, so "the last line is always X" says only that X is the last
thing that logs. The page's own §"Not root-caused" already established that
the bail itself is a normal, handled, self-healing path
(`jit/src/x64/driver.rs`, `crate::note_code_buffer_shortfall`); what it could
not know was that the line is not a marker of where execution stopped.

## What the class actually does

Single class, alone, `-Xmx 2g`, Windows, 2026-08-11:

| binary | collector | host | seconds | result |
|---|---|---|---:|---|
| `cratonvm-sbresid-20260811` (dev@9f9c03f20) | ZGC (default) | idle | 178.6 | 74/74, 0 failed |
| `cratonvm-sbresid-20260811` | ZGC (default) | idle | 203.7 | 74/74, 0 failed |
| `cratonvm-default-20260808f` (pre-JMX-monitor-fix) | Generational | 2 runs sharing the box | 259.1 | 74/74, 0 failed |
| `cratonvm-sbresid-20260811` | Generational | 2 runs sharing the box | 276.6 | 74/74, 0 failed |
| `cratonvm-sbresid-20260811` | Generational | 1 other run on the box | 220.8 | 74/74, 0 failed |
| `cratonvm-sbresid-20260811` | ZGC (default) | box carrying another session's full suite | 327-340 | 74/74, 0 failed |
| HotSpot 25 | — | idle | 7.0 | 74/74, 0 failed |
| HotSpot 25 | — | 3 runs sharing the box | 8.4 | 74/74, 0 failed |

The 2026-08-11 `JarFile`-accessor fix
(`fixed-bugs/jarfile-accessors-stat-the-file-on-every-call-FIXED-20260811.md`)
is **not** measurable on this class either way: paired runs against it ranged
251 s / 380 s / 611 s against the unfixed binary's 204 s / 327 s / 340 s on a
host whose load was moving under both, which is a spread, not a result. That
is the expected answer — the fix is about repeated jar scans and this class
starts no embedded container — and it is recorded here so nobody re-derives
it. If the class ever needs a real ratio, take it on a quiet box.

Three things follow.

1. **It is not stuck, on any binary tested.** The pre-fix 08-08f binary — one
   of the family the three HANG rows were produced on — completes the class in
   259 s when it is not fighting a full shard for the box.
2. **It sits on the 300 s boundary.** 178 s idle, 220-277 s with company. A
   shard running several classes in parallel pushes it over, which is exactly
   the pattern the page recorded: HANG in three full-suite runs, PASS in
   isolated reruns (182 s, 369 s) and in some full-suite runs (08-02 azure,
   07-28 rerun). "Interleaved with clean PASSes" is what a budget-boundary
   class looks like, not what a deterministic hang looks like.
3. **The real number is the ratio.** 178-204 s against HotSpot's 7.0 s on the
   same idle host is ~25-29x. That is a throughput fact about Spring's
   `ApplicationContextRunner`/`OnBeanCondition` bootstrap under this VM, and it
   belongs to the general interpreter-throughput work, not to a page about a
   `Stream.collect` pin bug.

## What changed in the tree

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s
`Get-EffectiveClassTimeoutSec` now carries a measured budget for this class,
with the same justification as the ~20 entries already there (Jackson,
RabbitMQ, the contextrunner cluster): the class is CPU-bound and finishes, and
reporting a false HANG at 300 s hides whatever its real result is. The ratio
itself is not papered over by that entry — it is stated in the entry's comment
and owned elsewhere.

## The GC-guard framing, settled

The original page recorded a `cratonvm::gc::guard` "checkcast receiver points
into RECLAIMED memory" hit naming `MultiValueMap`, then set it aside under the
standing "a reclaim-guard hit is about the address, not the object" caveat,
and a later revision argued the caveat had been misapplied because
`location=young TO-space` with `span=…+0x0` is a different claim. That
revision was right, and the 08-07 fix confirmed it: the object really was a
reference that had never been remapped, held unpinned across a moving young
collection. Both halves of that lesson are already carried by
`reference_reclaim_guard_hit_is_about_the_address_not_the_object` — read the
guard's `location=` field before applying the caveat.

## Affected classes

- `module/spring-boot-pulsar` —
  `org.springframework.boot.pulsar.autoconfigure.PulsarAutoConfigurationTests`:
  **PASS**, 74/74 (2 skipped), no failure and no hang on any binary or
  collector tested on 2026-08-11.
