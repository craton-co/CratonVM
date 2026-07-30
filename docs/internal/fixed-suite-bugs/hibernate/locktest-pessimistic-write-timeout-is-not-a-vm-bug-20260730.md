# `jpa.lock.LockTest` pessimistic-write timeout — NOT a CratonVM defect

**Status: RETIRED 2026-07-30 — real-JDK parity, no VM bug.**

`org.hibernate.orm.test.jpa.lock.LockTest` was carried as a CratonVM `FAIL`
("narrowing overshoot trend"). It is not one. **HotSpot fails the same test, the
same way, on the same host.**

## Evidence

Eclipse Adoptium JDK 25.0.3.9 vs the task release binary, identical fixture,
identical `CratonRunner` invocation, `-Xmx2g`, quiet host:

| Runtime | `@@RESULT` |
|---|---|
| HotSpot | `found=23 started=15 ok=14 failed=1 aborted=0 skipped=8 ms=10269` |
| CratonVM (JIT, default) | `found=23 started=15 ok=14 failed=1 aborted=0 skipped=8 ms=25573` |

Every count matches — `found`, `started`, `ok`, `failed`, `aborted`, `skipped`.
The single failure is the same method on both runtimes:

```
org.opentest4j.AssertionFailedError: execution exceeded timeout of 5000 ms
    at ...LockTest.testFindWithPessimisticWriteLockTimeoutException(LockTest.java:127)
```

## Why it fails on both

`LockTest.java:127` wraps the body in `assertTimeout(Duration.ofSeconds(5), ...)`.
Inside that budget the test opens **three nested JPA transactions**, builds an
`EntityManagerFactory`-backed session, and performs a deliberate
`JAKARTA_LOCK_TIMEOUT = 0` pessimistic-write acquisition that must round-trip to
H2 and come back as a lock exception. That is a wall-clock assertion on
first-run, entirely cold code — no warm-up, one execution.

Run as an isolated single method the margin is even thinner, and HotSpot misses
it by a wider relative margin (3 consecutive runs, `MethodRunner`):

| Run | HotSpot `test_ms` |
|---:|---:|
| 1 | 6521 |
| 2 | 6033 |
| 3 | 5785 |

All three exceed the test's own 5000 ms budget on a real JDK.

## Conclusion

The assertion is calibrated to a faster/quieter machine than this build host.
It is a **fixture/host-speed artifact**, not a VM defect, and CratonVM's
behaviour here is exact real-JDK parity — the correct outcome by the
project's real-Java standard.

Do not spend VM work on this class. If the suite must be green, the fix belongs
in the harness (skip or re-baseline the wall-clock assertion), not in CratonVM.

CratonVM is ~2.5x slower than HotSpot on this class overall (25.6 s vs 10.3 s
wall) — that gap is real and is tracked with the rest of the Hibernate
throughput work, but it is not what makes this test red.
