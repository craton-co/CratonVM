# `PcapWriteHandlerTest` hangs again — a reopening, most likely of the same real-socket family the 2026-08-13 fix closed

**Status: OPEN (reopened).** Measured 2026-08-16, commit `3ef3eb744`, Windows
host, `cratonvm.exe` release build, G1. Flagged HANG on generational, G1, and
ZGC in a same-day full 657-class 3-collector suite run; this page isolates it
(`--shards 1`, one class alone per process, no other collector running
concurrently) and cross-checks against HotSpot 25 on the same host.

This class was previously RESOLVED — see
`fixed-suite-bugs/netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md`
(from here on, "the FIXED doc"), which reported the class going from 18/25 to
25/25 (24/25 on a loaded box) after fixing three residuals: a UDP
`bind()`-breaks-fd-identity defect that silently dropped all inbound
datagrams (§1c), a global-zero-quiet-period `shutdownGracefully` bug that
skipped every `handlerRemoved` hook process-wide (§2), and a re-filed
(non-bug) throughput note about the 4 GiB test (§4). Today the class hangs
again, past the harness's 180s cap, with almost no progress.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 25 | 25 | 0 | **5.9s** |

Isolated (single class, single shard, no other collector or class running
concurrently) — not a full-suite contention artifact. HotSpot passes the
entire class, including the historically-slow 4 GiB test, in under 6 seconds
(consistent with the FIXED doc's own measurement of ~1.15s for that one test
under HotSpot C2). This is a clean regression from the FIXED doc's own
closing state (24-25/25 in 104-170s), not a re-measurement of the same
known-slow test tipping over a timeout.

## What ran before it went silent

The raw log contains exactly two lines of output for this class between the
JUnit launcher startup and the 180s kill, 10.2 seconds apart:

```
03:08:49.064 [main] WARN  i.n.handler.pcap.PcapWriteHandler - Unable to write UDP packet to PCAP. Payload of size {} exceeds max size of 65507
03:08:59.229 [main] WARN  i.n.handler.pcap.PcapWriteHandler - Unable to write UDP packet to PCAP. Payload of size {} exceeds max size of 65507
```

This is netty's own known `{}`-placeholder logging bug (already recorded as
not-a-CratonVM-defect in the FIXED doc §3), and it is only emitted by two
tests: `udpLargeByteBufPayload` and `udpLargeDatagramPayload`
(`PcapWriteHandlerTest.java:220,251`) — both pure in-memory `EmbeddedChannel`
tests with **no real sockets**. Both evidently ran and passed (no
`@@TESTFAIL` for either). Then the process produced **zero** further output
for the remaining ~170 seconds until the harness killed it.

25 tests exist in the class; the other 23 include:

* three real-socket UDP round-trips (`udpV4SharedOutputStreamTest`,
  `udpV4NonOutputStream`, `udpV4NoGlobalHeaderOutputStream`, all calling the
  shared `udpV4()` helper, which binds a real `NioDatagramChannel` server and
  client via `MultiThreadIoEventLoopGroup` and blocks on
  `clientChannel.writeAndFlush(datagram).sync()` /
  `eventLoopGroup.shutdownGracefully().sync()`) — exactly the send/receive
  and graceful-shutdown shapes the FIXED doc's §1c and §2 were about,
* three real-socket TCP round-trips (`tcpV4SharedOutputStreamTest`,
  `tcpV4NoGlobalHeaderOutputStream`, `tcpV4NonOutputStream`, via the `tcpV4()`
  helper — the exact helper the FIXED doc's §2 traced `handlerRemoved` through),
* the known-slow `writePcapGreaterThan4Gb` (measured at ~104s on CratonVM by
  the FIXED doc — on its own, comfortably inside today's 180s budget),
* and various `Embedded*`/zero-length/exception-path tests that do not touch
  real sockets.

**None of the real-socket or embedded test methods in this class carry their
own `@Timeout`** (confirmed by grep — the file has no `@Timeout` annotations
at all), so a hang on any blocking `.sync()`/`.await()` call has no
per-method guard; only the harness's flat 180s process cap eventually kills
it.

## Reopening verdict

This is presented as a **likely reopening of the same defect family**, not a
newly diagnosed bug, and not proven to be byte-for-byte the same root cause:

* The evidence fits: the class ran cleanly to 24-25/25 immediately after the
  2026-08-13 fix; it is fully silent-hung today; the two tests it did
  complete are the two that never touch a real socket; and the untouched
  majority is exactly the real-socket UDP/TCP send-and-shutdown surface the
  fix's §1c (UDP `bind()` fd-identity) and §2 (`shutdownGracefully`
  zero-quiet-period skipping `handlerRemoved`) were about. Both of those
  failure modes manifest as an indefinite hang on a `.sync()`/`.await()` with
  no timeout to catch it, which is exactly what is observed.
* What is **not** established here: no bisection was run against the commit
  history between the FIXED doc's `760e22328`-plus-fix state and today's
  `3ef3eb744` to find what actually regressed, and no probe was re-run to
  distinguish §1c from §2 (or a new, unrelated defect) as the specific
  mechanism. The FIXED doc's own repro probes — the `openDatagramChannel`
  `isOpen()` three-call check for §1a/1b, the `state=WRITING` vs
  `state=CLOSED` check for §2, and the raw-`DatagramChannel`-sender vs
  netty-sender bisect for §1c — are the fastest way to tell which one broke,
  and are the recommended next step rather than re-deriving them from
  scratch.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.pcap.PcapWriteHandlerTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

## Related

* `fixed-suite-bugs/netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md`
  — the original fix this page believes has regressed; its §1c/§2/§6 repro
  snippets are the fastest path to confirming which mechanism broke.
