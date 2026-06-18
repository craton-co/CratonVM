# ES-HANG-02 — `RestClient*IntegTests` hang against an embedded HTTP server (both JIT and `--nojit`)

**Status:** 🔴 **OPEN** — re-verified on current `dev` (2026-06-18): `RestClientSingleHostIntegTests`
still hangs (`rc=124`, only the `JUnit version 4.13.2` banner prints).
**Severity:** MEDIUM — small, self-contained cluster of network integration tests in `client/rest`.
**Baseline:** HotSpot JDK 25.0.1 runs them in ~2–3 s.

> **Why this is the only ES-suite doc kept here.** Its three former siblings are all resolved on
> current `dev` and their docs were removed as stale (2026-06-18):
> **ES-HANG-01** (Lucene `LuceneTestCase` JIT livelock) — fixed by `1cd0ab26` (same WeakHashMap-spliterator
> JIT ban as `kafka-bug-C`; `LuceneOnlyTest` now runs in ~8 s);
> **ES-FAIL-03** (`NativeAccessHolder` `catch (LinkageError)` "not honored") — re-verified: the catch *is*
> honored now, bootstrap continues to `NoopNativeAccess`, `ESTestCase` suites run;
> **ES-FAIL-04** (`cratonvm/internal/ArrayListSubList` missing `toArray(T[])`) — fixed + committed
> (`subList(a,b).toArray(new T[0])` works). This doc is the lone still-actionable ES item.

## Affected classes (`client/rest`)
- `org.elasticsearch.client.RestClientSingleHostIntegTests` — HotSpot `OK (13 tests)` in 2.7 s; CratonVM **HANG** (rc 124) under **both** default and `--nojit`.
- `org.elasticsearch.client.RestClientMultipleHostsIntegTests` — same.
- (`RestClientBuilderIntegTests` runs OK on CratonVM — it does not stand up the server the same way.)

## Why it is a socket/NIO bug (not JIT, not Lucene)
- These extend `com.carrotsearch.randomizedtesting`-based `RestClientTestCase`, **not** `LuceneTestCase`, so they are not the (now-fixed) JIT-livelock family.
- They **hang under `--nojit` too**, so it is not a JIT miscompile — it is in the HTTP/socket path.

## Mechanism
`RestClient*IntegTests` start an in-process Apache **httpcore `HttpServer`** bound to localhost and drive real HTTP requests through `RestClient`. HotSpot completes all requests; CratonVM never gets past suite start (only the `JUnit version 4.13.2` banner is printed, then the process is CPU/IO-stuck until the external timeout). The hang is in the embedded-server bind/accept or the client request round-trip on CratonVM's socket/NIO layer.

This is consistent with prior CratonVM async-socket / server-socket gaps noted elsewhere in the project (real server-socket accept + async channel transport). It needs a targeted socket-layer repro.

## Reproduce
```bash
export CLASSPATH="$(cat apps/elasticsearch/client/rest/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false --Xmx 1g \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientSingleHostIntegTests   # hangs (rc=124)
```
Next step: attach `cdb` to the hung process and inspect the stuck thread — whether it is in
`accept`/`select` (embedded-server side) or in the client request round-trip — then capture a minimal
embedded-`HttpServer` repro. (cdb recipe: `cdb -p <pid>` then `~* k`; symbolication is partial on the
release binary, so prefer the per-thread native stack of the CPU-bound thread.)

## Fix vs handoff
**Handoff** — belongs with the socket/NIO workstream. Low blast radius (a few integ tests).
