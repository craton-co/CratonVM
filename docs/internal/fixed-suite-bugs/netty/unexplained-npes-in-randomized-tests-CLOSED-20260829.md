# Three `NullPointerException`s in randomized/property tests — CLOSED 2026-08-29

## Status

**CLOSED.** All three classes are accounted for, and nothing on the page is
still open.

| class | verdict |
|---|---|
| `io.netty.util.internal.LongLongHashMapTest.randomOperations` | **A real VM defect, FIXED 2026-08-28** — `longlonghashmaptest-npe-spliced-ctor-this-not-a-gc-root-FIXED-20260828.md` |
| `io.netty.buffer.ByteBufDerivationTest.testMixture` | **Not a defect** — never reproduced again; see the soak below |
| `io.netty.resolver.dns.DnsQueryContextTest.writeQueryMustNotSendWhenIdSpaceExhausted` | **Not a defect** — same |
| `io.netty.handler.proxy.ProxyHandlerTest` (the page's "possibly related" note) | **Two real VM defects, FIXED 2026-08-29** — `proxyhandlertest-ipv6-scope-and-localized-socket-error-FIXED-20260829.md` |

Superseded page:
`known-issues/netty/unexplained-npes-in-randomized-tests-20260826.md`.

## The one that was real

`LongLongHashMapTest` was a call-carrying inline splice holding `this` in a
callee local that no oop map named, across a moving young collection —
`AbstractLongAssert.<init>`'s trailing `putfield longs` landing on the vacated
copy of an object the caller's own reference had already been rewritten to. The
whole write-up, the bisect and the instrument that found it are on the FIXED
page above; nothing about it is repeated here.

Re-confirmed on this branch's binary: **43/43 clean** (see the table below).

## The two that were not

The page's own last open item was: *"`ByteBufDerivationTest` /
`DnsQueryContextTest` not retried a third time; if either reappears, it stops
being explainable as one-off noise."* They were retried, and the retry was
built to be able to fail.

A zero on a quiet host would have been worth nothing here — the original
sighting came from a **contended 4-shard run**, and this tree has already had a
page retired on 640 quiet runs and then reopened by a loaded one. So the third
retry was run under real load, and the load was verified by its own effect on
wall time rather than assumed:

| batch | shape | runs per class | result |
|---|---|---:|---|
| 1 | solo, while a fat-LTO `cargo build` saturated the box | 3 | clean |
| 2 | 4 concurrent instances of the class, otherwise idle | 16 | clean |
| 3 | 8 concurrent instances + 24 CPU spinners | 24 | clean |
| | | **43** | **0 failures, 0 aborts** |

Batch 2 is reported but should be discounted: on a 32-core host, four
single-threaded JVMs is not contention, and its wall times prove it —
`LongLongHashMapTest` averaged 22.0 s against 21.3 s solo. Batch 3 is the one
that counts: the same class averaged **38.6 s, 1.8x its idle time**, so the
runs really were fighting for the machine.

Three classes x 43 runs, with two-thirds of them under verified load, and the
two 2026-08-27 quiet reruns already on the original page. Both classes are
closed as random-seed/contention noise from the original 4-shard run.

## The `ProxyHandlerTest` note was the most valuable thing on the page

The original page carried `io.netty.handler.proxy.ProxyHandlerTest` (8 of 47
parameterisations, `array lengths differ, expected: <0> but was: <1>`) as
*"possibly related, uncertain"*, kept only in case a shared root cause with the
NPEs turned up, and wrote it off as *"one extra byte arrived in an AUTO_READ
success-path check … plausibly just contention"*.

No shared root cause turned up — the NPE that was real is a GC/oop-map defect
with no data-length symptom near it. But **both halves of the write-off were
wrong**, and checking them is what this retirement actually bought:

* It is not contention. Re-run on a quiet host on this branch's binary it
  failed **8 of 47 on every run**, against HotSpot's 47/47 on the same host and
  classpath.
* No extra byte arrived. `expected: <0> but was: <1>` is not a byte count; it
  is `assertArrayEquals(EMPTY_OBJECTS, testHandler.exceptions.toArray())` at
  `ProxyHandlerTest.java:718` — the client recorded ONE exception where the
  test asserts none.

Two real VM defects were under it, both now fixed, both with a reach far past
netty: an IPv6 loopback address carrying a scope suffix that
`InetAddress.getByName` could not parse back, and socket `IOException`
messages carrying the OS's LOCALIZED text, which `SslHandler.ignoreException`
matches an English regex against. `ProxyHandlerTest` is now **47/47, 5/5
runs**. See
`fixed-suite-bugs/netty/proxyhandlertest-ipv6-scope-and-localized-socket-error-FIXED-20260829.md`.

The lesson is the cheap one: a "possibly related" note that is never checked is
a defect with a shelf. This one sat clear for three days on a reading of the
assertion message that opening `ProxyHandlerTest.java:718` would have
corrected.

(The page it cited for the contention explanation,
`timing-margin-fails-under-4shard-zgc-self-contention-20260826.md`, no longer
exists anywhere under `docs/` — it was retired without this reference being
updated. The name is spelled out here rather than linked.)

## What this page is worth keeping for

The correction it made to itself. The original analysis picked the wrong branch
of the test's `if`, reasoned carefully about how `LongLongHashMap` could diverge
from its `HashMap` oracle, and was wrong — the class under test is a pure
`long[]` open-addressing map with no reference in its hot path and **no
expression that can produce a null**. Reading the class instead of the
traceback is what turned "a netty correctness bug" into "CratonVM's own
execution of this loop", which is the half of the page the fix was actually
built on.

Its two nominations for the mechanism — the JIT's compilation of the hot loop,
or AssertJ's overload resolution — were both wrong too, and in an instructive
way: the defect was in neither party's code but in the *composition* of two
separately-sound changes (call-carrying inline splices, and ZGC suppressing the
conservative sweep under a proven-JIT coverage claim).

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.internal.LongLongHashMapTest
# and the same for io.netty.buffer.ByteBufDerivationTest and
# io.netty.resolver.dns.DnsQueryContextTest -- but run them CONTENDED
# (8 concurrent + CPU spinners), or the zero means nothing.
```
