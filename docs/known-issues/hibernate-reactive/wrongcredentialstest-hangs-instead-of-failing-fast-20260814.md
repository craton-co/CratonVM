# `WrongCredentialsTest` hangs 120s instead of failing fast — the only genuine non-crash residual across the full suite

**Status:** OPEN (2026-08-14). Found on Windows running the **full**
249-class hibernate-reactive suite (not a subset) in G1 and ZGC, 4 shards
each, binary at commit `6a206f689`.

## How this was isolated

The full suite's FAIL bucket looked large (177 classes on G1, 163 on ZGC),
but per-class raw-log triage (splitting each shard's `raw.log` at its
`@@RESULT` boundaries and checking each FAIL class's own segment against
the three known Testcontainers/Jackson-JIT-stall signatures — see
`testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`) found that
**176/177 (G1) and 162/163 (ZGC) are that same already-documented issue**,
not new regressions. Exactly one class in each variant did not match:
`org.hibernate.reactive.WrongCredentialsTest`, identical in both.

## Symptom

`WrongCredentialsTest` exists specifically to verify that opening a
session with bad DB credentials fails cleanly. Its `@BeforeEach` setup
*succeeds* (unlike the Jackson-JIT-stall pattern, where `before()` itself
times out) — the failure is in the test method itself:

```
@@TESTFAIL org.hibernate.reactive.WrongCredentialsTest testWithTransaction(VertxTestContext) FAILED
java.util.concurrent.TimeoutException: testWithTransaction(io.vertx.junit5.VertxTestContext) timed out after 120 seconds
	at org.junit.jupiter.engine.extension.TimeoutExceptionFactory.create(TimeoutExceptionFactory.java:31)
	...
```

`found=1 started=1 ok=0 failed=1` — this class has exactly one test
method, and it hangs for the full 120s JUnit timeout rather than
completing (successfully or with an assertion) quickly. The expected
behavior, given the class name and purpose, is that attempting a
transaction against a session opened with wrong credentials should fail
promptly with a clear authentication error — not hang.

## HotSpot-clean confirmation

Identical class, classpath, and args under stock HotSpot: **PASS,
9s wall (6781ms)**. CratonVM never completes it within 120s, on either
collector.

## Collector-independent

| variant | result |
|---|---|
| G1 | FAIL, `testWithTransaction` times out at 120s |
| ZGC | FAIL, `testWithTransaction` times out at 120s |

Identical failure mode on both — this is not a GC-specific defect (unlike
the SIGSEGV cluster found the same day, see
`zgc-specific-sigsegv-cluster-20260814.md`).

## Not yet done

No raw log inspection of *what* the test method is actually doing while
hung (no stack/thread dump captured), and no comparison against the
already-documented SASL/SCRAM handshake bug
(`vertx-pg-sasl-scram-handshake-fails-20260812.md`) or the JNA Docker-
strategy NPE — plausible this is a related connection/authentication-path
defect given the class's purpose (deliberately invalid credentials), but
that's a guess, not confirmed. Worth checking whether CratonVM's
Postgres-wire-protocol client ever receives/processes the server's
authentication-failure response at all, versus HotSpot's clean 9s
rejection.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
echo org.hibernate.reactive.WrongCredentialsTest > /tmp/one.txt
CV_BIN=bin/cratonvm-hibreactive-g1.exe bash run-hibernate-reactive-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --timeout 180 --out /tmp/repro
bash run-hibernate-reactive-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

## Related

- `docs/known-issues/hibernate-reactive/testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`
  — the dominant Windows-only pattern this class was ruled out against;
  everything else in the full suite's FAIL bucket is that issue.
- `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`
  — a different Postgres-auth-adjacent defect found earlier; not confirmed
  related, worth checking given both involve authentication paths.
- `docs/known-issues/netty/zgc-specific-sigsegv-cluster-20260814.md` — the
  other finding from this same day's full-suite run; unlike that one,
  this defect is GC-independent.
