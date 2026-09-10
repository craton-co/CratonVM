# Lane 5 — `java.util.concurrent`, `Thread`, and `Unsafe`

**Scope: 405 §1.4 shadows over 23 classes, from 345 registration sites.**
Prefixes: `java/util/concurrent/`, `jdk/internal/misc/`, `sun/misc/`,
`java/lang/Thread*`, `java/lang/VirtualThread*`, `jdk/internal/vm/`.

The smallest class count in the campaign and the highest concentration: 173 of
405 rows are two classes. Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first.
Method, preconditions, landing protocol:
[`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. Shape of the lane

```text
  91  jdk/internal/misc/Unsafe          25  java/util/concurrent/ForkJoinTask
  82  sun/misc/Unsafe                   22  java/util/concurrent/CopyOnWriteArraySet
  33  java/lang/Thread                  19  java/util/concurrent/ForkJoinPool
  30  jdk/internal/misc/ScopedMemoryAccess  16  CopyOnWriteArrayList
                                        15  ThreadPoolExecutor
```

`CompletionException`, `ExecutionException` and `RejectedExecutionException`
(16 each) are lane T's throwable registrar. Not yours.

`ConcurrentHashMap` and its seven view/iterator classes were retired in the
2026-09-09 Phase 3 wave (152 triples) and live in
`RETIRED_SHADOW_PHASE3_TRIPLES`. **Do not extend that table** — it is a
historical wave. Read it as your playbook, then work in
`RETIRED_SHADOW_L5_TRIPLES`.

## 2. The 173 `Unsafe` rows are the lane's headline, and they are bucket A/B

This is counter-intuitive enough to state plainly: **most of `Unsafe` is not
`ACC_NATIVE` in the JDK.** Real `jdk.internal.misc.Unsafe` implements the great
majority of its surface in Java, delegating to a small core of true intrinsics.
The census confirms it — these rows have image `Code`, so §1.4 applies and the
remedy is available.

Retiring them means the JDK's own delegation runs. That is usually right and has
one sharp edge:

> **The JDK emulates sub-word atomics with byte-offset arithmetic.**
> `compareAndSetByte`, `compareAndSetShort`, `getAndAddByte` and friends are
> implemented in Java on top of a 4-byte CAS, computing shifts and masks from
> the field offset. Retiring them hands your memory model a computation over
> *your* offsets and *your* endianness.

So the probe requirement is specific: for every sub-word atomic, exercise **all
four byte positions within a word**, on both a field and an array element, and
on a `boolean`/`byte`/`short`/`char` each. A probe that tests one offset tests
the one case that works by accident.

`sun/misc/Unsafe` (82) is the legacy façade over `jdk/internal/misc/Unsafe`
(91) — bucket B, one unit. Retire them together or the two disagree about the
same memory.

`jdk/internal/misc/ScopedMemoryAccess` (30) is the FFM session-liveness
gatekeeper and pairs with L4's `MemorySessionImpl`. **Coordinate with L4**; a
liveness check split across two lanes is a liveness check nobody owns.

## 3. `java/lang/Thread` (33) — and the two probes you must not trust

This lane's instruments are the flakiest in the campaign, and the numbers are on
record:

- `JdkOnlyPlatformProbe` and `VtHandoffProbe` produced deltas of
  **0, −2, 0, 0, +2, +2** across six A/Bs of *different* changes, none of
  which touched virtual threads.
- **The same binary gives opposite verdicts.** `cratonvm-p8.exe` differed from
  HotSpot when it was the trial arm, and was byte-identical to HotSpot when the
  next A/B used it as the control. No binary attribution is possible on this
  probe.
- **Two fields drift, not one.** On the `vthreads` line HotSpot reports
  `handoffs=64 allJoined=true`; this VM has been observed at `handoffs=` **50,
  60, 63 and 64** and `allJoined=` **true and false**, in independent
  combinations. A reader who chases only `handoffs` -- as this campaign's
  earlier note did -- is looking at the wrong half about as often as the right
  one.

Therefore: **measure a vector's noise floor before explaining its delta.** Run
the unchanged binary against itself N times first. A negative delta on either of
those probes is not a win until it survives that, and the ops page names both
for exactly this reason.

Related traps in this area:

- **A suite failure that passes alone is leakage** — check isolation before
  believing a mode-specific failure. Concurrency manufactures them.
- **Host load flips pass/fail, not only timings.** A workspace test diff taken
  under load invents regressions.
- **Zero watchdog acks means compiled code or a native, not "no native".**
- **`native_registry` `invocations` saturates on a warm loop**, so a large count
  is not a magnitude and a repeated count is not a plateau.

## 4. `ForkJoin*` (44 rows) — one known real gap lives here

`RJdkForkJoin` fails with `AssertionError: CountedCompleter leaves: 128`. That
is one of the 41 `AssertionError`s in the corpus and it is **a genuine
behavioural difference, not a null field** — L7's triage routed it here.

Treat it as this lane's first correctness target rather than a retirement:
`CountedCompleter`'s pending-count protocol is the kind of thing a hand-written
native gets subtly wrong, and fixing it may make several `ForkJoinTask` rows
retirable as a side effect. Price it before the bulk waves.

`CopyOnWriteArrayList`/`CopyOnWriteArraySet` (38) are the opposite: plain value
semantics over an array snapshot, no VM-filled state, and a good first
mechanical wave. Probe iterator snapshot isolation — an iterator obtained before
a mutation must not see it — and `addIfAbsent`/`addAllAbsent` equality rules.

## 5. Traps specific to concurrency measurement

- **Run A/B arms concurrently, not sequentially.** ABBA on this host read 1.9×
  for a flag that costs nothing.
- **Three interleaved pairs is not a result** on this host, and a ratio between
  two rows of one run is still a load artefact.
- **Count the work units, not the wall time.** A `HANG` cell is a claim about
  your timeout: three of them were a 300 s limit on a 490 s class that was still
  logging one second before the kill.
- **Three passes is not an absence proof.** A test written to fail passed 3/3
  and then failed 60% of 30 runs.
- **Never a timing claim from this lane.** See L0 §5.

## 6. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run — and remember `invocations` saturates.
2. Probe + HotSpot oracle, configured like the VM under test. No build needed.
3. Fill `RETIRED_SHADOW_L5_TRIPLES`, sorted and unique.
4. Build token (L0 §5); one build per wave.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B (arms concurrent), `--jdk-only` corpus, `SUITE=all` at
   `TIMEOUT=600`, `all`-arm count. Establish the noise floor for any vector you
   intend to cite.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 7. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the sub-word atomics covered at all four byte offsets, `ScopedMemoryAccess`
settled jointly with L4, and every cited delta backed by a noise floor from the
same probe.
