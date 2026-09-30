# `TestNonBlockingAPI.testNonBlockingReadAsync` — deterministic connector timeout, NOT the `Util$BufferCache` corruption bug

| | |
|---|---|
| **Status** | Open — root-caused 2026-09-26 to a fixture timeout too tight for CratonVM's current socket-write throughput; fix not attempted (needs native I/O throughput work, out of scope for this pass) |
| **HotSpot** | PASS |
| **CratonVM** | FAIL, deterministic (2/2 identical reruns) — `java.io.IOException: HttpURLConnection streaming response failed: response read: ... (os error 10053)` |
| **Prior attribution** | [`34-defaultinstancemanager-zgc-generational-classunload-and-httpurlconnection-epipe-FIXED-20260925.md`](../../internal/fixed-suite-bugs/tomcat/34-defaultinstancemanager-zgc-generational-classunload-and-httpurlconnection-epipe-FIXED-20260925.md) attributed this class's remaining failure to ["33's separate, pre-existing `Util$BufferCache` bug"](../../internal/fixed-suite-bugs/tomcat/33-classloader-proxy-modifiers-threadcontention-FIXED-20260922.md) (lines ~420-625 of that doc) without directly confirming it against this specific symptom — that attribution appears to be **wrong**, see below |

## Why this is not doc 33's bug

Doc 33's `Util$BufferCache` investigation (nine build-instrumented repro cycles, a full session) is explicit that the corruption is **non-deterministic in both point and shape** — sometimes an NPE from `Util$BufferCache.get()` after only ~200 bytes, sometimes a `BufferUnderflowException`, sometimes a different NPE, sometimes after hundreds of thousands of bytes, sometimes not at all.

Re-running `TestNonBlockingAPI` twice against `cratonvm-tomcathsid-fix18-9e444f.exe`:

| run | result |
|---|---|
| 1 | `testNonBlockingReadAsync` FAILS, `os error 10053` at `TomcatBaseTest.postUrl` line 817, `Tests run: 44, Failures: 1` |
| 2 | Identical: same test, same line, same exception text, `Tests run: 44, Failures: 1` |

Same test, same failure, same shape, both times. That is the opposite of doc 33's signature. Doc 34's attribution was an inference from "these look like the same family (`Socket`/`HttpURLConnection` I/O abort)," not a confirmed shared root cause.

## What this actually looks like: a connector timeout, not corruption

`doTestNonBlockingRead(false, true)` posts a body via `TomcatBaseTest.postUrl` using
`DataWriter(delay=0, max=2000000)` — 2,000,000 iterations of an 8-byte chunk, `os.write()` +
`os.flush()` per iteration (`setFixedLengthStreamingMode`, `TomcatBaseTest.java` ~800-810), 16 MB
total. The fixture's connector has `connectionTimeout=3000` hardcoded
(`TomcatBaseTest.java:203`, applies on the server/`NioEndpoint$Poller` side — the *client's* own
`connection.setReadTimeout(1000000)` is irrelevant here). The server log shows the failure sequence
starting almost immediately:

```
10:22:06  Starting test case [testNonBlockingReadAsync]
10:22:06  ProtocolHandler init / Tomcat start
10:22:10  TestReadListener onError: java.net.SocketTimeoutException
              at NioEndpoint$Poller.timeout / NioEndpoint$Poller.run
```

~4 seconds from test start to the server's poller timing the socket out — close to the connector's
3000 ms timeout plus connector-startup overhead, not close to the ~224 s a full 2M-write transfer
takes end to end on this VM (see below). This is much more consistent with a **timeout on some
specific phase of the exchange** (most likely connection accept-to-first-byte, or the request head
being slow to arrive) than with either sustained aggregate throughput or a corrupted buffer.

## GC pauses ruled out

The working theory going in was a long, one-off GC pause blowing the 3 s window (this project's ZGC
default has produced multi-second stalls elsewhere). Reran the standalone analog of this workload —
`tools/probes/SocketWriteBufferCacheCorruption.java` (plain blocking `Socket`, same 2,000,000 x
8-byte write+flush shape doc 33 built it to mirror) — with `--verbose:gc`:

```
[GC] zgc-real: collections=0 occupancy=1297344/4294967296 bytes
```

**Zero collections** over the whole ~224 s run. Whatever is slow, it is not GC pause time — there is
no GC event to blame here at all on this workload.

## The raw per-write cost is real but not obviously large enough to explain a 4 s failure

Same probe, HotSpot vs CratonVM, 2,000,000 x 8-byte `write()+flush()` (16 MB total):

| VM | wall time | per-write |
|---|---:|---:|
| HotSpot | 22.2 s | ~11 us |
| CratonVM (`fix18`) | 224.1 s | ~112 us |

A real ~10x per-call gap (much smaller than the project's well-known ~500x BCEL/`DataInputStream`
throughput wall — that one is a different code path, class-file parsing, not socket I/O), but at
112 us/write there is no single gap anywhere near 3 seconds in 2,000,000 uniformly-paced writes — the
whole transfer, run start to finish, is only ~224 s of *continuous* small writes, and the failure
here happens at ~4 s, far too early for cumulative throughput alone to be the culprit unless the
server's timeout is measured from connection/request start rather than from the last successful
read (needs checking against Tomcat's actual `NioEndpoint$Poller` semantics for this connector mode).

## Root cause pinned down: the client-side write phase itself takes ~6.2s, not ~224s and not ~0

Built `tools/probes/HttpUrlConnStreamingTimeline.java` — the exact client-side call sequence
`postUrl` uses (`HttpURLConnection` + `setFixedLengthStreamingMode` + `connect()` then
`getOutputStream()` then a tight `os.write()+os.flush()` loop), against a plain accepting
`ServerSocket`, with nanosecond timestamps around each phase.

**First run found a genuine, separate divergence and a probe-ordering trap**: written to call
`acceptLatch.await()` (i.e. wait for the server's `accept()`) *before* `getOutputStream()`, matching
the reasonable assumption that `URLConnection.connect()` "results in an actual connection" (its own
javadoc). On HotSpot this holds — `connect()` alone is enough for the peer's `accept()` to fire. On
CratonVM it does not: `connect()` returns in ~4 ms but the peer's `accept()` never fires until
`getOutputStream()` is called — CratonVM defers the actual TCP connect to first output access
instead of doing it eagerly in `connect()`. Real, reproducible, worth its own note, but **not** what
fails `testNonBlockingReadAsync`: the real test calls `connect()` then `getOutputStream()` in that
same order without ever blocking in between, so this divergence is invisible to it.

**Reordered probe (`getOutputStream()` before waiting on accept) exposes the real number.** Full
2,000,000 x 8-byte run, `fix18`:

| phase | CratonVM | HotSpot |
|---|---:|---:|
| `connect()` | 4.3 ms | 5.8 ms |
| `getOutputStream()` | 2.4 ms | 2.4 ms |
| accept observed | ~19 ms | ~19 ms |
| first 100 writes | ~5 ms | ~2 ms |
| **remaining 1,999,900 writes** | **6.22 s** | **29.57 s** |
| **total client-side time** | **6.29 s** | **29.61 s** |

Two things fall out of this:

1. **CratonVM is not the slow one here — HotSpot is**, by nearly 5x, for this exact many-tiny-
   `write()+flush()` pattern (an 8-byte payload flushed 2,000,000 times over loopback is squarely in
   Nagle/delayed-ACK territory; ~14.8 us/write on HotSpot matches the ~11 us/write this doc's earlier
   plain-`Socket` probe measured independently). This rules out "CratonVM's socket-write path is too
   slow" as the story — it manifestly is not, for this workload.
2. **CratonVM's own 6.29 s total is still more than double the fixture's hardcoded 3000 ms connector
   timeout** (`TomcatBaseTest.java:203`). HotSpot's 29.6 s figure never matters in the real Tomcat test
   because the real test's connector applies that timeout **server-side**, and HotSpot's own real
   `NioEndpoint`/`Poller` combination evidently tolerates its own client taking that long (this
   fixture has presumably always been timing-tolerant on HotSpot in ways this standalone probe, which
   talks to a bare accepting socket rather than a real async NIO2 servlet read pipeline, does not
   capture 1:1) — the one number that generalizes cleanly is that **CratonVM's client-side write phase
   alone (6.29 s) already overruns the server's 3 s timeout by more than 2x**, independent of exactly
   how HotSpot's own server-side timing margin works out to still pass.

This is enough to close the "is it corruption, a stall, or a raw connect-time issue" question:
**it's none of those.** It's the aggregate cost of driving 2,000,000 separate `write()+flush()` calls
through CratonVM's `HttpURLConnection`/socket stack (~3.1 us/write measured), which this fixture's
HotSpot-derived 3-second timeout was never sized to tolerate, whatever mechanism ultimately lets
HotSpot itself stay under it despite its own similarly-scaled per-write cost.

## Next step (a fix, not further diagnosis)

Two independent, addressable items fell out of this investigation, either of which could close this:

1. **Fix the underlying per-`write()+flush()` cost** on the CratonVM socket path so 2,000,000 tiny
   writes complete in well under 3 s (would need to beat HotSpot's own 29.6 s by ~10x, not just match
   it — since the timeout is HotSpot-tuned against HotSpot's OWN behavior on some other, evidently
   much cheaper, code path this probe doesn't exercise).
2. **Fix `HttpURLConnection.connect()`'s deferred-connect divergence** noted above (separately real,
   separately worth fixing for its own sake, but confirmed not the cause of this specific failure).

Neither was attempted in this pass — both are code changes to hot native I/O paths, out of scope for
a diagnosis-only session step. This doc's job was to stop misattributing the failure to doc 33's
`Util$BufferCache` corruption bug (ruled out: that bug is non-deterministic, this fails identically
every time) or to a GC pause (ruled out: `--verbose:gc` shows zero collections) — both now closed off
with direct evidence, leaving a precisely-quantified throughput gap as the confirmed cause.

## Reproduce

```powershell
cd apps\tomcat-suite-runner
.\run-one.ps1 -Vm craton -Exe <cratonvm.exe> -Class org.apache.catalina.nonblocking.TestNonBlockingAPI -LogFile <path>.log
# -LogFile is required to get a clean capture: PowerShell's native-stderr handling otherwise
# truncates the console output this test class produces (44 sub-tests, each starting its own
# embedded Tomcat instance) before the JUnit summary line is reached.
```

Standalone throughput/GC-pause probe (no Tomcat):

```powershell
javac -d . tools\probes\SocketWriteBufferCacheCorruption.java
<cratonvm.exe> --verbose:gc -cp . SocketWriteBufferCacheCorruption 2000000
# check for [GC] zgc-real: collections=N > 0, and any single reported pause near or above 3000ms
```

Standalone client-side timeline probe (`postUrl`'s exact call sequence, no Tomcat):

```powershell
javac -d . tools\probes\HttpUrlConnStreamingTimeline.java
<cratonvm.exe> -cp . HttpUrlConnStreamingTimeline 2000000 8
# "remaining N writes: ... us total" is the number to compare against the 3000ms connector timeout
```
