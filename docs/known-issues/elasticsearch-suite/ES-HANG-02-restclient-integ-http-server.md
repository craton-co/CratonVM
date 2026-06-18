# ES-HANG-02 — `RestClient*IntegTests` hang against an embedded HTTP server

**Status:** OPEN
**Severity:** MEDIUM — small, self-contained cluster of network integration tests in `client/rest`.
**VM:** `cratonvm.exe` from `dev`. **Baseline:** HotSpot JDK 25.0.1 runs them in ~2–3 s.
**Date:** 2026-06-18

## Affected (`client/rest`)
- `org.elasticsearch.client.RestClientSingleHostIntegTests` — HotSpot `OK (13 tests)`; CratonVM **HANG** (rc 124) under **both** default and `--nojit`.
- `org.elasticsearch.client.RestClientMultipleHostsIntegTests` — same.
- `org.elasticsearch.client.RestClientGzipCompressionTests` — same family.

## Why distinct from the other findings
- Extend `RestClientTestCase` (carrotsearch randomizedtesting), **not** `LuceneTestCase` → not the JIT-livelock family (ES-HANG-01).
- Hang under `--nojit` too → not the JIT bug.

## Mechanism
These start an in-process Apache **httpcore `HttpServer`** on localhost and drive real HTTP requests through `RestClient`. HotSpot completes; CratonVM never gets past suite start (only the `JUnit version` banner prints) and is stuck until the external timeout — the hang is in the embedded-server bind/accept or the client request round-trip on CratonVM's socket/NIO layer. Consistent with prior CratonVM async-socket / server-socket gaps.

## Reproduce
```bash
export CLASSPATH="$(cat client/rest/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false --Xmx 1g \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientSingleHostIntegTests   # hangs
```
Next step: attach `cdb` to the hung process to see whether the stuck thread is in `accept`/`select` (server side) or the client request, and capture a minimal embedded-`HttpServer` repro.

## Fix vs handoff
**Handoff** — independent of the dominant blockers; belongs with the socket/NIO workstream. Low blast radius.
