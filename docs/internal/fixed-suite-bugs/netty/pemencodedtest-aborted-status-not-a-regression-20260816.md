# CLOSED — `PemEncodedTest` "ABORTED on all 3 collectors" was the runner's status label, and the label is fixed

**Status:** ✅ CLOSED 2026-08-17 on `fix/netty-sni-ocsp-rld-residuals-20260817`.
Retired from `docs/known-issues/netty/`; originally written 2026-08-16/17 at
commit `3ef3eb744` as "NOT A BUG — closing the question", to head off future
sessions re-investigating it as a reopening of
`ssl-cert-validation-residuals-FIXED-20260813.md`.

The original page's diagnosis was right and is preserved below. What it left in
place was the thing that produced the false alarm: the suite runner's per-class
status field. That is now fixed, so the same class cannot manufacture the same
scare again.

---

## Why it looked like a regression

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
after the 2026-08-13 fix. The two aborted methods hit
`org.opentest4j.TestAbortedException: Assumption failed` from
`Assumptions.assumeFalse` inside `PemEncodedTest.testPemEncoded`
(`PemEncodedTest.java:50`) — a normal, intentional JUnit skip (the OpenSSL-only
parameterizations skipping when the OpenSSL provider path isn't the one under
test), identical on both VMs.

## The actual explanation

The suite runner's per-class status field reported `ABORTED` whenever a class
had `aborted > 0` and `failed == 0`. A class where 2 of 3 methods are expected
to (and correctly do) hit `assumeFalse` therefore *always* showed class status
`ABORTED`, on every collector, every time — including on a run where nothing
was wrong. That is what that full-suite run recorded: not new breakage, just
the steady-state "1 ok / 2 a" result reported through a status label that reads
alarmingly at a glance.

---

## What was fixed (2026-08-17)

An assumption skip is not an abort in any sense a reader cares about — Surefire
reports those as *skipped*, and JUnit itself treats a class whose only
non-successes are assumption skips as passing. The runner now says so.

* **`CratonRunner.java`** classifies each aborted test by walking its cause
  chain for `org.opentest4j.TestAbortedException` (matched by name, so the
  runner keeps compiling against a classpath that does not export opentest4j
  directly, and a wrapped/nested abort still counts). Those are counted into a
  new `assumed=` field on the `@@RESULT` line and reported as
  `@@TESTSKIP … assumption` with no stack trace, instead of `@@TESTFAIL` with
  one. `assumed=` is APPENDED after the existing keys — the shell parses
  `@@RESULT` with `grep -o 'key=[0-9]*'`, so an extra key is invisible to
  anything that does not look for it, and an older `CratonRunner.class` that
  omits it degrades to `0`, i.e. exactly the previous behaviour.
* **`run-netty-suite.sh`** labels the class `ABORTED` only when
  `aborted > assumed` — i.e. only when at least one abort was something other
  than an assumption skip.

Verified, same class, both VMs:

| arm | before | after |
|---|---|---|
| HotSpot 25 | `ABORTED 3/1/0/2` | `PASS 3 found / 1 ok / 0 failed / 2 aborted` |
| CratonVM G1 | `ABORTED 3/1/0/2` | `PASS 3 / 1 / 0 / 2` |

The `aborted` COLUMN still reports 2 in both, so nothing is hidden — the counts
are unchanged and visible; only the one-word verdict now matches what they mean.
A class that aborts for a real reason (an initializer blowing up, a
`TestInstantiationException`) still reports `ABORTED`, because those are not
`TestAbortedException`.

`apps/` is gitignored wholesale, so both edits live only in the working tree of
the main worktree and are not part of this branch's diff. Recorded here so the
next session knows the label's semantics changed and can restore it from this
page if the fixture is ever lost.

## Related

- `ssl-cert-validation-residuals-FIXED-20260813.md` — the fix this page
  confirms is still holding.
- `defaultthreadfactorytest-zgc-timeout-20260816.md` — the other class
  investigated in the same original session pass; unlike this one, that one
  *does* reproduce a real regression in isolation.
- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md`,
  `sniclienttest-sni-refusal-alert-FIXED-20260817.md`,
  `ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817.md` — the pages
  retired alongside this one.
