# ObjectCleanerTest: an intermittent AbstractMethodError on Comparator.compare, not reproduced live

## Status

**OPEN, unreproduced.** Observed once, in a full 733-class Netty suite run
under host contention (9 concurrent CratonVM processes: 3 GC arms x 3
internal shards). Did not reproduce in 16 follow-up isolated/concurrent
attempts. Recorded as a real but rare, load-sensitive symptom rather than a
pinned defect.

## What was observed

`io.netty.util.internal.ObjectCleanerTest`, one shard of a Generational-GC
full-suite run (2026-09-04):

```
@@RESULT io.netty.util.internal.ObjectCleanerTest found=3 started=3 ok=1 failed=2 aborted=0 skipped=0 ms=1109
```

2 of the class's 3 test methods (`testCleanup`, `testCleanupContinuesDespiteThrowing`,
`testCleanerThreadIsDaemon`) failed with:

```
java.lang.AbstractMethodError: method java/util/Comparator.compare(Ljava/lang/Object;Ljava/lang/Object;)I has no Code attribute
```

## Why this doesn't look like a real Comparator call

`common/src/test/java/io/netty/util/internal/ObjectCleanerTest.java` has **no
`Comparator` usage anywhere in its source** — no sorting, no `PriorityQueue`,
no explicit `compare()` call. The three test methods only: spawn a thread,
register an `ObjectCleaner` callback, join, and poll `System.gc()` until the
callback fires; and (`testCleanerThreadIsDaemon`) call
`Thread.getAllStackTraces()` and check `isDaemon()`.

The error text — "has no Code attribute" on an interface method — matches
this project's documented conservative-root-scan false-positive family
(Family-A / G30, see `G30-1-the-silent-reference-slot-coercion-20260817.md`
and the `is_object_address` "Family-A fix" comment in
`gc/src/gen_heap.rs`): a conservative scan lands on interior bytes that
happen to decode as a plausible-looking object header, gets treated as a
real object, and a method dispatch against its (fake) class lands on an
abstract/interface method slot that was never meant to be invoked directly.
`Comparator` is exactly the shape this would produce — a functional
interface with no concrete implementation body reachable from a
mis-identified receiver.

This is a hypothesis based on the error's shape and this project's own
prior-documented defect family, **not confirmed** — no receiver, class, or
call site was captured for this specific occurrence.

## Reproduction attempts — all clean

16 attempts total, none reproduced:

- 6x isolated (`--Xmx 1g`, no other process running)
- 4x isolated with a small heap (`--Xmx 64m`, to increase GC frequency/pressure)
- 6x concurrent (9 simultaneous instances on one host, mimicking the original
  full-suite contention)

All 16 completed `found=3 ok=3 failed=0` in 660-1350ms.

## What would be needed to pin this down

- Catch a live occurrence and dump the receiver's raw header bytes /
  `CRATONVM_DBG_LAYOUT=1` output to identify what class the misdecoded
  object actually belongs to and which caller triggered the scan.
- Since it did not reproduce under either isolated OR concurrent conditions
  matching the original run, the trigger may be a specific GC-cycle timing
  window (e.g. a scan landing mid-collection) rather than raw host load —
  worth trying with `CRATONVM_DBG_COERCION=1` armed across a large batch of
  runs to catch one live, per this project's own `[inv=0 lies]` /
  `[noise floor 1st]` conventions: measure the actual flake rate over many
  more runs before spending further effort explaining a single occurrence.

## Repro

```bash
cd apps/netty-suite-runner
CV_BIN=<binary> JDK=<jdk25 home> ./run-netty-suite.sh \
  --list <(echo io.netty.util.internal.ObjectCleanerTest) --shards 1 --out /tmp/repro
# Has not reproduced in isolation; the one known occurrence was under a
# 733-class, 9-concurrent-process full-suite run (3 GC arms x 3 shards).
```
