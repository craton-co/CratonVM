# `PemEncodedTest` "ABORTED on all 3 collectors" is the documented FIXED state, not a reopening

**Status: NOT A BUG — closing the question, no doc needed in the OPEN sense.**
Written 2026-08-16/17, commit `3ef3eb744`, Windows host, `cratonvm.exe`
release build, to head off future sessions re-investigating this as a
reopening of
`docs/internal/fixed-suite-bugs/netty/ssl-cert-validation-residuals-FIXED-20260813.md`.

## Why this looked like a regression

A same-day full 657-class 3-collector parallel suite run recorded
`PemEncodedTest` with class-level status `ABORTED` on all three collectors
(generational, G1, ZGC). The FIXED doc above lists `PemEncodedTest` as one of
seven rows resolved on 2026-08-13, which made "ABORTED on all 3, every time"
read as a plausible reopening.

## Isolation result

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.ssl.PemEncodedTest\n' > /tmp/pemgroup.txt
./run-netty-suite.sh --list /tmp/pemgroup.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/pemgroup.txt --hotspot --shards 1 --out runs/repro
```

| arm | found | ok | failed | aborted |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 3 | 1 | 0 | 2 |
| HotSpot 25 (isolated) | 3 | 1 | 0 | 2 |

Both are `1 ok / 2 aborted` — byte-for-byte the same shape, and exactly what
the FIXED doc's own table recorded as the correct, matching-HotSpot result
after the 2026-08-13 fix (`PemEncodedTest | 1 ok / 2 f | 1 ok / 2 a ✅ | 1 ok
/ 2 a`, i.e. CratonVM-after and HotSpot columns already both read "1 ok / 2
a"). The two aborted methods are `testPemEncodedOpenSsl()`-style tests
hitting `org.opentest4j.TestAbortedException: Assumption failed` from
`Assumptions.assumeFalse` inside `PemEncodedTest.testPemEncoded`
(`PemEncodedTest.java:50`) — a normal, intentional JUnit skip (OpenSSL-only
parameterizations skipping when the OpenSSL provider path isn't the one under
test), identical on both VMs.

## The actual explanation

The suite runner's per-class status field reports `ABORTED` whenever a class
has `aborted > 0` and `failed == 0`. A class where 2 of 3 methods are expected
to (and correctly do) hit `assumeFalse` therefore *always* shows class status
`ABORTED`, on every collector, every time — including on a run where nothing
is wrong. That is what today's full-suite run recorded: not new breakage, just
the same steady-state "1 ok / 2 a" result reported through a status label that
reads alarmingly at a glance. Nothing here contradicts or reopens the
2026-08-13 fix.

## Related

- `docs/internal/fixed-suite-bugs/netty/ssl-cert-validation-residuals-FIXED-20260813.md`
  — the fix this doc confirms is still holding.
- `defaultthreadfactorytest-zgc-timeout-20260816.md` — the other class
  investigated in the same session pass; unlike this one, that one *does*
  reproduce a real regression in isolation.
