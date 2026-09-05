# `VthreadProbe` hangs, about one run in five, with no test harness involved

**Status:** open, and reproducible on demand.

## Reproducer

No cargo, no test harness — the VM alone:

```bash
CRATONVM_JAVA_HOME=<jdk25> ./target/release/cratonvm \
  -c vm/tests/resources/vthread_probe/classes VthreadProbe
```

Ten consecutive runs on Windows, one after another in the same shell:

| runs | elapsed | output |
|---|---|---|
| 8 | 2–4 s | `counted=10000 ok=true` |
| 2 | killed at 120 s | nothing at all |

## It is a hang, not slowness

The distribution is BIMODAL with nothing in between: healthy runs finish in
2–4 s, and a bad run produces no output and never finishes. There is no middle
ground, which is what separates a hang from a workload that is merely slow
under contention.

**It is not load.** One failure occurred with three background compilers
running, and three of the passes occurred with exactly the same three. A run at
six compilers passed. Machine load does not predict it.

## What it is not

* **Not the test harness.** The reproducer above spawns nothing and pipes
  nothing. `vm/tests/vthread_probe_regression.rs` did have a real defect — a
  `try_wait` poll loop over piped stdio that never drained, which deadlocks on a
  child that outruns the 64 KiB pipe — and that is fixed. It is a different
  bug, and it was not this one: this child writes 175 bytes.
* **Not a heavy tail.** Twenty runs under added load on Linux gave 20/20
  correct at 3.36–26.39 s, which read as a tail and prompted raising the test's
  cap from 60 s to 300 s. That was the wrong reading of the wrong sample: 20
  runs simply did not catch the hang. The cap is back to 60 s, because a cap
  cannot fix a hang — raising it only delays the report.

## Where to look first

The poll loop this displaced carried a comment naming the suspect, and it is
worth taking seriously:

> helps when the v-thread scheduler regresses to a 1-carrier livelock

A hung run produces NO output, not even the probe's first line, so the stall is
before or during the point where the 10 000 virtual threads are dispatched
rather than in the counting.

## What would settle it

A native stack from a hung child. The failure rate is high enough (2 in 10)
that attaching to a stuck process is practical rather than a stakeout: run the
reproducer in a loop, and when one exceeds ~30 s take a stack of every thread.
That names the carrier state directly, where the counters cannot.
