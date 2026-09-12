# c1/c2 tier run, 2026-09-12: single-arm FAILs that look load/timing-sensitive rather than tier-specific

## Status
**Not investigated further — catalogued so they aren't mistaken for
tier-specific defects.** Every class below FAILed in exactly one of the two
JIT-tier arms (c1: `CRATONVM_C2_SUPERSEDE=0`; c2: `CRATONVM_JIT_FORCE_C2=1`)
and PASSed in the other, on the same binary, same fixture, same 651-class
run. Per this suite's own repeatedly-learned lesson (`nonpassed-class-census.md`:
"the shard count is part of the measurement"; `ssl-renegotiation-emulation-limits.md`'s
`testPost` note: "treat a red as noise unless it reproduces on a quiet host"),
an asymmetric single-run result on a class whose failure shape is itself
timing/socket-shaped is not evidence of a tier-dependent bug — it's exactly
the shape host contention produces. None of these were re-run to check.

## The classes

| class | arm it failed in | shape |
|---|---|---|
| `org.apache.catalina.authenticator.TestFormAuthenticatorB.testPostNoContinueWithCookies` | c2 only | `SocketTimeoutException: Read timed out` (`NioSocketImpl.timedRead`) |
| `org.apache.catalina.authenticator.TestFormAuthenticatorC.testPostWithContinuePostRedirectWithCookies` | c1 only | `SocketTimeoutException: Read timed out` (`NioSocketImpl.timedRead`) — identical exception shape to `TestFormAuthenticatorB` above, different sibling class (A/B/C split the same form-auth test bodies into separate JUnit classes), different sub-test |
| `org.apache.catalina.filters.TestRateLimitFilter.testUnexposeHeaderAndEnforcedRateLimitWith4Clients` | c1 only | `AssertionError: expected:<200> but was:<0>` — a `0` HTTP status from a concurrent 4-client test reads as "no response received at all," the same connection-refused/no-response shape a starved host produces |
| `org.apache.catalina.mapper.TestMapperPerformance.testPerformance` | c1 only | `AssertionError: 5481` — a bare failed timing-threshold assertion, same family as the already-documented `TestResponsePerformance`/`TestAsyncMessagesPerformance` relative-timing races in `windows-local-environment-artifacts.md` |
| `org.apache.catalina.valves.TestAccessLogValve.test[74: Name[pct-t-begin:umlaut_time_S], Type[text]]` | c1 only | `AssertionError: Access log line empty after 1002 milliseconds` — a wait that times out at 1002ms against what reads as a ~1000ms nominal wait; see the tick-quantization note below |

`org.apache.catalina.tribes.test.channel.TestMulticastPackages` (c2 only) is
**not** included here — it's the already-documented, HotSpot-reproducing
Tribes multicast family in `tribes-multicast-family-still-environmental.md`
("1 of 5" flaky is exactly its documented shape), just not triggered in the
c1 run this time.

## Why these read as noise, not tier effects

- Both `TestFormAuthenticator{B,C}` fail with the byte-identical exception
  shape (a read timeout on a POST) in *opposite* arms — if this were a real
  C1-vs-C2 defect it should be reproducible in the same arm both times, not
  split one-and-one.
- `TestRateLimitFilter`'s `expected:<200> but was:<0>` under 4 concurrent
  clients is indistinguishable, from the assertion alone, from one of the
  four client connections simply not getting serviced in time — the same
  ambiguity this suite's own `loadavg1` CSV column exists to resolve for
  `HANG` (see `run-tomcat-suite.md` §3), just manifesting as a `FAIL` instead
  of a timeout here.
- `TestMapperPerformance` and the already-catalogued
  `TestResponsePerformance`/`TestAsyncMessagesPerformance` are the same
  shape: a test asserting one code path beats another (or beats a fixed
  budget) under CratonVM's execution profile, which this session's other
  suites have repeatedly found sensitive to host load rather than a fixed
  property of the binary.
- `TestAccessLogValve`'s single failing case waits **1002ms** against what
  the test's own naming (`umlaut_time_S`) implies is a sub-second nominal
  wait — 2ms over a round window is the shape Windows tick-quantized timed
  waits produce, not a logic defect (see the memory note on Windows timed
  waits being tick-quantized except for the high-resolution timer). Not
  confirmed for this specific test, but the magnitude fits.

## Not yet done
- Re-run each of these 5 classes standalone, several times, on an otherwise
  quiet host, to see whether any of them reproduce outside a 651-class
  concurrent sweep. None of that was done this session — this page exists
  only to keep them from being reported as tier-specific findings, not to
  claim they're understood.
