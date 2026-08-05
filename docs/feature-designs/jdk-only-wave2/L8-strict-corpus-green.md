# L8 — Criterion 6: strict corpus green

**Owns:** `probes/`, `regression-suite/`, `scripts/` — no production source
**Gated on:** nothing. Runs beside everything.
**Effort:** L (mostly running things and triaging)

## Goal

Contract §11's sixth acceptance criterion is "the strict corpus is green". It
was the **unmeasured half** of the acceptance criteria until 2026-08-04, and
measuring it immediately found that `--jdk-only` **could not start a thread** —
`UnsatisfiedLinkError: java/lang/Thread.run()V`, for a method whose real
bytecode the image plainly declares. Every `new Thread(…)` died, every
`ExecutorService` had no live workers, and a workload that joined on one *hung*
rather than failed.

The lesson is the lane: **criterion 6 is not a formality to tick once the list
is done. It is where the defects are.**

## Current state

Two breadth probes exist and both are byte-identical to HotSpot 25 under
`--jdk-only` and `--real-jdk`:

* `probes/JdkOnlyCensusLoadProbe.java` — collections, interfaces, streams,
  `Properties`, io, nio, net, executors, text.
* `probes/JdkOnlyBreadthProbe.java` — reflection, method handles, lambdas,
  regex, time, text formatting, charsets, zip, serialization, atomics, locks,
  exceptions, class loading, records, bignum.
* `probes/JdkOnlyIcHotProbe.java` — JIT-hot dispatch, used for refusal counters.

That is a floor, not a corpus. Three small probes dispatched 401 of 11,909
registered native slots.

## Steps

1. **Widen the corpus.** The suites are the real test: H2, Hibernate, Spring
   Boot, Tomcat. Run each under `--jdk-only` with a HotSpot control and triage
   what breaks. Expect the same shape as the thread defect — things that have
   never been exercised in strict mode.
2. **Take the censuses from those runs**, not from the probes. This is also L6's
   and L7's blocker: `image_declaring_method` is workload-independent, but
   `invocations`, `requested_by` and the class-origin counts are only as wide as
   what ran.
3. **Make the probes CI-runnable** so a strict regression fails a build rather
   than waiting for someone to look.
4. Add sections for what the probes do not cover: JNI, agents/attach, security
   providers, `ProcessBuilder`, virtual threads.

## Probe-writing rules, learned the hard way

* **Bound every blocking call.** The first two strict census runs *hung*;
  `timeout` `SIGKILL`ed them, the exit hook never wrote the artefact, and the
  result was indistinguishable from a job still running.
  `ServerSocket.setSoTimeout`, `Socket.connect(addr, ms)`,
  `Future.get(n, SECONDS)`.
* **Print values, not `ok`.** The interesting strict failures were *wrong
  numbers* — `FileChannel.size()` returning `0` — not exceptions. A section that
  prints "ok" cannot show one.
* **Tally at the end** (`sections=N failed=M`) so a truncated run is visible.
* **Always run a HotSpot control at the same time.** It caught a bug in my own
  GC probe that would otherwise have been filed as a VM defect, and it is what
  proved the socket-timeout hang was real rather than host load.
* **Check exit status.** A `timeout` kill prints a truncated transcript that
  reads exactly like a clean short run.

## Known open, found by this lane

[Bounded socket operations hang about one run in five](../../known-issues/bounded-socket-operations-hang-about-one-run-in-five.md)
— pre-existing, mode-independent (the `dev` binary hangs at the same rate under
`--real-jdk`), and **not** host load: HotSpot ran 12/12 clean at the same load
average. Five candidate call sites named in the record; the cheapest next step
is a thread dump at the moment of the hang.

## Done when

The suites run under `--jdk-only` with their failures either fixed or filed, the
probes are in CI, and the censuses driving L5/L6/L7 come from a real workload.
