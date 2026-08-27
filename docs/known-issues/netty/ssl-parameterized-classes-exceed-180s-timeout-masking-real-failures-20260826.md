# `OpenSslEngineTest` and its interop siblings exceed the harness's 180s per-class cap, hiding real per-case assertion failures inside a HANG

## Status
**OPEN**, not root-caused as a single defect — likely two separate things bundled
under one HANG status. 2026-08-26.

## Context

Complete 657-class local netty suite run (G1/ZGC/Generational, `dev` HEAD `9ba39b4c1`).
Four classes reported HANG (killed at the harness's 180s per-class wall cap,
`process-died rc=124`), identically across all three GC arms:

- `io.netty.handler.ssl.OpenSslEngineTest`
- `io.netty.handler.ssl.JdkOpenSslEngineInteroptTest`
- `io.netty.handler.ssl.OpenSslJdkSslEngineInteroptTest`
- `io.netty.handler.ssl.ReferenceCountedOpenSslEngineTest`

All four extend `SSLEngineTest`, whose test methods are `@ParameterizedTest`s over
a large protocol × cipher × delegate × useTasks × useTickets matrix — dozens to low
hundreds of cases per method, each doing a real (in-JVM, no network) TLS handshake.
`OpenSslEngineTest` alone logs at least 5 distinct `@@TESTFAIL` cases before the raw
log is truncated by the timeout kill, so the class is not stuck — it is grinding
through its matrix and simply doesn't finish 180s of that grind, taking down every
result inside it (including any real PASSes) into an undifferentiated HANG.

## One confirmed real assertion failure inside the grind

```
@@TESTFAIL io.netty.handler.ssl.OpenSslEngineTest [1] OpenSslEngineTestParam{
    type=Direct, protocolCipherCombo=...TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    delegate=false, useTasks=true, useTickets=false} FAILED
org.opentest4j.AssertionFailedError: expected: <true> but was: <false>
	at io.netty.handler.ssl.SSLEngineTest.mySetupMutualAuth(SSLEngineTest.java:1302)
	at io.netty.handler.ssl.SSLEngineTest.testMutualAuthDiffCerts(SSLEngineTest.java:662)
```

`mySetupMutualAuth` asserts the mutual-TLS handshake it just drove actually completed
(`assertTrue(handshakeComplete)` or similar, at line 1302) before the caller checks
anything about *which* certs were exchanged. Not yet determined whether this
particular parameterization (`useTasks=true`, i.e. delegated async task execution
during the handshake) fails on HotSpot too, or is CratonVM-specific.

## Two separate open questions

1. **Is 180s simply too short for this class family?** These classes were very
   likely never a HANG on whatever host/timeout this suite's `netty-nonpassed-latest.txt`
   baseline (2026-08-13) was captured against — they aren't in that 43-class list.
   `run-hib.sh`'s hibernate suite has a `class-overrides.tsv` mechanism for exactly this
   (a per-class timeout floor); `run-netty-suite.sh` has the same
   `class-overrides.tsv` plumbing (`CLASS_TIMEOUT_OVERRIDE`) already wired in — these
   four classes are simply not in the table yet.
2. **Is `mySetupMutualAuth`'s `useTasks=true` failure a real CratonVM defect?** Not
   isolated or cross-checked against HotSpot yet. `useTasks=true` routes the SSL
   handshake through `SSLEngine`'s delegated-task execution path (`Runnable` tasks the
   engine hands back for the caller to run, typically off-thread) — if that path
   behaves differently under CratonVM (e.g. a delegated task never completing before
   the assertion checks), that would explain both this failure and part of why the
   class runs long enough to hit the cap.

## Next steps

1. Add a per-class timeout floor (600s+) for the four classes above to
   `apps/netty-suite-runner/class-overrides.tsv`, so a full run reports each class's
   actual FAIL/PASS breakdown instead of one HANG covering everything.
2. Once un-capped, get the real per-class `ok`/`failed` counts and cross-check
   `testMutualAuthDiffCerts[useTasks=true]` (and any other real FAILs it turns up)
   against stock HotSpot on the same host.

## Repro

```bash
cd apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 600 \
  C:/craton/CratonVM/target/release/cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseG1GC \
  @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.OpenSslEngineTest
```

## Related files
- `apps/netty/handler/src/test/java/io/netty/handler/ssl/SSLEngineTest.java`
- `apps/netty/handler/src/test/java/io/netty/handler/ssl/OpenSslEngineTest.java`
- `apps/netty-suite-runner/class-overrides.tsv`
