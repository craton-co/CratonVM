# hibernate-reactive on Azure Linux — 3 residual non-passing classes (post-Docker-Desktop-fix triage)

**Status:** OPEN, partially characterized (2026-08-13). Found on Azure host
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

## 3. `MultithreadedInsertionWithLazyConnectionTest` — genuine HANG, not yet root-caused

Confirmed HANG in isolation (`--shards 1`, still hits the 180s cap,
`rc=124`) — not a contention artifact. The class spins up 12 concurrent
Vert.x verticles that each lazily acquire a pooled DB connection and
insert entities. The raw log shows all 12 verticles cleanly reach and
breach a `start` countdown latch (i.e. all 12 threads launch and begin
their work):

```
63 - vert.x-eventloop-thread-2: Reached latch 'start', current countdown is -1
63 - vert.x-eventloop-thread-2: Everyone has now breached 'start'
... (all 12 threads breach 'start') ...
```

but the log then stops — no thread ever reaches the corresponding `end`
latch that the *previous*, passing multithreaded test in the same shard
did reach (compare: `Reached latch 'end', current countdown is 0` /
`Everyone has now breached 'end'` / `Verticle stopped ...` appear for the
prior class but never for this one). This points at a real hang somewhere
in the lazy-connection-acquisition-under-concurrency path specific to this
test's `InsertEntitiesVerticle`, not a startup/setup issue.

**Not yet done**: HotSpot cross-check (this Azure copy of
`run-hibernate-reactive-suite.sh` doesn't support `--hotspot`, unlike the
netty harness's copy — would need porting that flag over first), and no
stack/thread-dump was captured at the hang point. Until both are done this
is a credible-but-unconfirmed CratonVM defect, not a certified one.

## Repro

```bash
# on azureuser@20.80.105.49, /data/cratonvm/apps/hibernate-reactive-suite-runner
printf '%s\n' org.hibernate.reactive.techempower.TechEmpowerTest \
  org.hibernate.reactive.it.LocalContextTest \
  org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest > /tmp/repro.txt
CV_BIN=bin/cratonvm-hibreactive-zgc bash run-hibernate-reactive-suite.sh --list /tmp/repro.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro-out
```
Deterministic for all 3, individually reproduced with no shard contention.

## Related

- `docs/known-issues/hibernate-reactive/testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`
  — the dominant Windows-only blocker these 3 classes were originally
  hiding behind; largely resolved by running on real Linux Docker instead.
- `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`
  — did NOT reproduce anywhere in this 195-class Linux run; appears to
  have been either fixed or was itself an artifact of the original Azure
  run's environment/timing, not confirmed either way.
