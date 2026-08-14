# hibernate-reactive on Azure Linux — 3 residual non-passing classes (post-Docker-Desktop-fix triage)

**Status:** OPEN. Items 1 & 2 are a harness gap (not CratonVM). Item 3 was
**re-characterized on 2026-08-13**: it is **not a hang** — it is a ~12-15x
THROUGHPUT deficit that JUnit's `@Timeout(120)` converts into a failure. Both
gaps the first triage listed as "not yet done" (HotSpot cross-check, stack dump)
are now closed. Originally found on Azure host
`azureuser@20.80.105.49` rerunning the 195-class Windows non-passed list
(binary at commit `d68f0f3c7`, `-XX:+UseZGC`, real Docker/Linux).

## Context

A prior run on Windows (Docker Desktop) found 0/238 classes passing,
dominated by a Testcontainers/Jackson JIT-stall bug (see
`testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`). Rerunning
the same 195-class subset on real Linux Docker instead reversed that
almost completely: **188/195 PASS (96.4%)** — confirming the Windows
failures were Docker-Desktop-environmental, not CratonVM defects.

Of the 7 non-PASS classes in the 4-shard run: 2 are `NOTESTS` (expected —
abstract/base classes with no `@Test` methods), and 2 more
(`MultithreadedIdentityGenerationTest`, `MultithreadedInsertionTest`)
turned out to be **shard-contention flakiness** — both PASS cleanly when
rerun alone (`--shards 1`). This doc covers the **3 that are still
non-passing even in isolation**.

## 1 & 2. `techempower.TechEmpowerTest` / `it.LocalContextTest` — NOT a CratonVM bug, a harness gap

Both fail identically, even run individually:

```
io.netty.channel.ConnectTimeoutException: connection timed out after 60000 ms: localhost/127.0.0.1:8088
	at io.netty.channel.nio.AbstractNioChannel$AbstractNioUnsafe$1.run(AbstractNioChannel.java:308)
	...
```

Port `8088` is not the Testcontainers-provisioned Postgres port (that's
always a random high port, e.g. `34288` in the passing runs) — it's a
**hardcoded local HTTP server port**. Both `TechEmpowerTest` (a TechEmpower
benchmark-suite test) and `it.LocalContextTest` (an integration test under
`org.hibernate.reactive.it`) are written to exercise a full running HTTP
endpoint of the application, which this fork-per-class CratonRunner
harness never starts — it only stands up the reactive DB session, not an
HTTP server. This is the same class of gap documented for quarkus's
`@QuarkusTest` classes needing a curated application bootstrap the flat
harness can't provide — **not a CratonVM defect**, just two classes this
harness shape can't run meaningfully. No further action needed unless the
harness gains HTTP-server bootstrap support.

## 3. `MultithreadedInsertionWithLazyConnectionTest` — NOT a hang; a ~12-15x throughput deficit

Re-run 2026-08-13 on current dev with both of the gaps the first triage listed
as missing. **The "hang" characterization was wrong**, and so was the evidence
offered for it.

### HotSpot cross-check: the harness CAN run this class

The first triage could not do this because the Azure copy of
`run-hibernate-reactive-suite.sh` has no `--hotspot` mode. It did not need
porting — `/data/hrb1-run.sh` already accepts `HOTSPOT` as its binary argument
and drives the same `CratonRunner` with the same `@common.args`:

```
HOTSPOT:  found=2 started=2 ok=2 failed=0 aborted=0 skipped=0 ms=16108   (rc=0, 18s wall)
```

So this is a CratonVM defect, not a harness gap — unlike items 1 and 2 above.

### It reaches the `end` latch. Every thread. Every run.

The first triage's central claim was "no thread ever reaches the corresponding
`end` latch". That is not what the log shows, on either binary. All 12 verticles
reach `end` and every one of them stops:

```
60 - vert.x-eventloop-thread-N: Reached latch 'end', current countdown is 11
   ... 61, 61, 61, 62, 101, 102, 102, 104, 104, 121 ...
121 - vert.x-eventloop-thread-N: Reached latch 'end', current countdown is 0
     vert.x-eventloop-thread-*: Verticle stopped ...InsertEntitiesVerticle@...
```

What actually happens is that JUnit's `@Timeout` fires first:

```
java.util.concurrent.TimeoutException: testIdentityGenerator(io.vertx.junit5.VertxTestContext)
        timed out after 120 seconds
    at org.junit.jupiter.engine.extension.TimeoutExtension.interceptTestMethod
```

and the `IllegalStateException: Session/EntityManager is closed` seen on two
threads is **collateral, not cause** — it appears at t=126s, i.e. after the 120s
timeout has already torn the context down. A 180s process cap made the whole
thing look like `rc=124`; at a 300s cap it is a plain test failure.

### Current dev is already better than the recorded binary

| binary | result | ms |
|---|---|---|
| `d68f0f3c7` (the one the triage used) | ok=0 failed=2 | 248 425 |
| current dev | **ok=1** failed=1 | 183 888 |

### Collector-insensitive

One run each, same class, current dev:

| | ok/failed | ms |
|---|---|---|
| default (ZGC) | 1/1 | 183 888 |
| `-XX:+UseGenerationalGC` | 1/1 | 215 400 |
| `-XX:+UseG1GC` | 0/2 | 246 287 |
| `-XX:+UseZGC` | 1/1 | 236 605 |

All in one band against HotSpot's 16 s, so this is not a GC defect and no
collector is a workaround.

### Where the time goes — and three things it is NOT

`--stack-sample-ms=250` (the time-weighted sampler; `--stack-dump-on-timeout`
ranks by CALL COUNT instead and must not be read as a profile), 2 772 records,
aggregated by leaf frame:

| | share |
|---|---|
| `java/util/concurrent/CompletableFuture` (uniComposeStage, internalComplete, uniWhenCompleteStage, newIncompleteFuture, tryPushStack, …) | ~28% |
| `org/hibernate/reactive/engine/impl/Cascade` | ~9% |
| `DefaultFlushEntityEventListener` isUpdateNecessary / performDirtyCheck | ~7% |
| `AsyncTrampoline$TrampolineInternal.unroll` | ~3% |

Diffuse across the reactive-composition path — no stall, no single hot method.
Three specific hypotheses were tested and **refuted**:

* **Not the JIT entry machinery.** `jit_entries=87 567 564`, but at 0.44 M/s
  against the 1.5 M/s of the netty family where entry bookkeeping WAS shown to
  be first-order (`adaptive-bytebuf-allocator-throughput-20260812.md`). Entry
  cost is real here but not saturating. `band_scans=0 band_words=0`, so no band
  scanning fires at all. (`cache_hits=0 (0.0%)` is expected — already recorded
  as always-zero in `conservative_roots.rs`, not a new finding.)
* **Not thread-identity instability.** `AsyncTrampoline.unroll` bounds its own
  recursion with a captured `java.lang.Thread` compared by reference (visible in
  `lambda$unroll$0(Ljava/lang/Thread;...)`), so an unstable
  `Thread.currentThread()` identity would make it recurse forever.
  `probes/ThreadIdentityProbe.java` says it is stable — `ref== true` back-to-back,
  across a GC-provoking churn, on the main thread, a plain `Thread`, and a pool
  thread, on both VMs.
* **Not a deadlock or a lock cycle.** The watchdog dump (`--stack-dump-on-timeout=75`)
  shows 13 distinct threads, all making progress, none blocked on a monitor.

### One concrete oddity, worth its own look

Two JIT compiles bail on a code-buffer estimate, and the sizes are extreme —
roughly **250x bytecode-to-machine-code expansion**, where 5-15x is normal:

```
method="...AbstractReactiveFlushingEventListener.logFlushResults:(...)V"  code_len=194  wanted=52041
method="io/vertx/pgclient/impl/PgRow.get:(Ljava/lang/Class;I)Ljava/lang/Object;"  code_len=948  wanted=229308
```

`PgRow.get` is on the per-column, per-row hot path. It fires only twice, so it
is not a compile storm, but a 229 KB method body is an I-cache problem on its
own and the expansion factor suggests an inlining or lowering bug. Not chased
further here.

### What this now is

A throughput defect on the `CompletableFuture`-composition path, in the same
family as the netty and H2 interpreter-throughput work rather than a discrete
bug — which is why it is filed rather than fixed in session. The threads finish
in waves (~60s, ~101-104s, ~121s) because the connection pool serializes 12
verticles over work that each holds a connection for tens of seconds instead of
HotSpot's ~1-2. Raising the class's `@Timeout` would turn the failure into a
slow pass and hide it; the number to move is the 12-15x.

## Repro

```bash
# on azureuser@20.80.105.49, /data/cratonvm/apps/hibernate-reactive-suite-runner
printf '%s\n' org.hibernate.reactive.techempower.TechEmpowerTest \
  org.hibernate.reactive.it.LocalContextTest \
  org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest > /tmp/repro.txt
CV_BIN=bin/cratonvm-hibreactive-zgc bash run-hibernate-reactive-suite.sh --list /tmp/repro.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro-out
```
Deterministic for all 3, individually reproduced with no shard contention.

For item 3 specifically, the 2026-08-13 re-characterization used a one-class
runner (`/data/hrb1-run.sh <tag> <bin|HOTSPOT> <class> <timeout> [vm flags...]`,
host-local) rather than the shard harness, because it accepts `HOTSPOT` as the
binary and so gives the oracle arm for free:

```bash
C=org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest
bash /data/hrb1-run.sh h3-hotspot HOTSPOT               $C 300      # 2/2 in 16s
bash /data/hrb1-run.sh h3-dev     /data/cvm-h3-dev      $C 300      # 1/2, ~184s
# where the time goes (time-weighted; aggregate the LAST depth= line per record):
bash /data/hrb1-run.sh h3-samp    /data/cvm-h3-dev      $C 150 --stack-sample-ms=250
# thread states mid-run (one dump per nested interpreter ENTRY - ranks by call
# count, NOT a profile; it aborts the process after dumping):
bash /data/hrb1-run.sh h3-wd      /data/cvm-h3-dev      $C 300 --stack-dump-on-timeout=75
```

`probes/ThreadIdentityProbe.java` is the refuted-hypothesis arm and runs
standalone on either VM.

## Related

- `docs/known-issues/hibernate-reactive/testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`
  — the dominant Windows-only blocker these 3 classes were originally
  hiding behind; largely resolved by running on real Linux Docker instead.
- `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`
  — did NOT reproduce anywhere in this 195-class Linux run; appears to
  have been either fixed or was itself an artifact of the original Azure
  run's environment/timing, not confirmed either way.
