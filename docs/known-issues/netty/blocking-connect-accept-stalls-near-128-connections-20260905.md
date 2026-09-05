# Blocking connect/accept stalls between 96 and 128 connections

**Status: OPEN, 2026-09-05.** Found while building F3's acceptance curve for
`performance/socket-transfer-per-call-costs-20260904.md`. **Not caused by that
work** — see the exoneration below, which is the first thing to re-run if you
doubt it.

## The symptom

`probes/SelectorScalingProbe.java` opens `N + 1` loopback connections in a
plain sequential loop — `SocketChannel.open(addr)` then `server.accept()`, one
pair at a time, each accepted socket registered `OP_READ` on a `Selector` — and
then runs a one-byte echo round trip against the first connection.

| N idle connections | CratonVM | HotSpot 25.0.3+9 |
|---|---|---|
| 8 | passes | passes |
| 64 | passes | passes |
| 256 | **STALLS in setup** | passes |
| 2048 | not reached | passes |

The stall is in **connection setup, between 96 and 128**, not in the traffic
that follows: the probe prints a progress line every 32 connections and the last
one printed is `connecting 96/256`. It never reaches `setup complete`.

**It is a stall, not slowness.** `Get-Process cratonvm` shows CPU flat at 13.3 s
across an 8-second window — the process is blocked, not spinning. That check
matters here because this tree has a recorded case of exactly the opposite
diagnosis ([[hashmap-hang-is-debug-slowness-not-deadlock]]): confirm which one
you have before reasoning about causes.

## Why this is not the socket fast-I/O work

The decisive run, and it is cheap:

```bash
CRATONVM_SC_SCRATCH=0 CRATONVM_SC_BB_SLOTS=0 \
  CRATONVM_SEL_READY_CACHE=0 CRATONVM_SEL_FAST_KEYS=0 \
  ./target/release/cratonvm.exe -cp <probe> SelectorScalingProbe
```

**It stalls identically.** Those four switches revert F1, F3, F4, F5 and F6 to
their pre-change behaviour at runtime, so with every behaviour change disabled
the stall persists. What is left of that commit is a census counter and one
added `NativeContext` accessor, neither of which can block a connect.

The same probe at N=8 and N=64 passes on BOTH arms with the checksum HotSpot
produces (4594), so the path works and simply does not scale.

## Why it matters more than a probe failure

This is the shape an HTTP keep-alive server has. A server that cannot get past
~128 concurrent connections does not report an error — **it hangs**, which is
indistinguishable at the harness level from the throughput walls this tree has
already had to reclassify (`compression-cluster-testhugedecompress-180s-...`,
`quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-...`). Any HTTP
cluster whose fixture opens more than ~128 connections would present as a
timeout with no exception and no diagnostic.

## What is NOT yet established

The cause. Candidates worth testing in this order, none of them confirmed:

* the listen backlog — the probe passes `bind(addr, 4096)` explicitly, and the
  loop accepts each connection before opening the next, so at most one should
  ever be pending. If the backlog argument is dropped somewhere this would still
  not obviously explain it, which is why it is first: it is the cheapest to
  refute;
* a fixed-size table or handle exhaustion in `tcp_registry` /
  `selector_register`, which `try_clone()`s a duplicate handle per registration
  — so N registrations hold ~3N sockets;
* a Windows-specific limit in the poll path. **Untested on Linux**: the Azure
  host has the same binary built and the probe is committed, so the one-line
  next step is running it there. If it passes on Linux the search narrows to the
  WSAPoll path immediately.

Do not file a cause on this page without running the arm that supports it — the
above are candidates, not a diagnosis.

## Repro

```bash
javac -d /tmp/p probes/SelectorScalingProbe.java
# IDLE_COUNTS in the probe selects the sweep; {8} and {64} pass, {256} stalls
timeout 240 ./target/release/cratonvm.exe -cp /tmp/p SelectorScalingProbe
# RC=124 with the last stderr line `[probe] connecting 96/256`
```
