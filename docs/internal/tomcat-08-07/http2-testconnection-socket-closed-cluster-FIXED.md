# HTTP/2 test-connection `Socket is closed` — broader than DoHead (2+ classes)

**Status: FIXED (2026-07-13, branch `fix/dohead-family-regressions-v2-20260713`).**
The root cause identified in the 2026-07-13 note below (synthetic
`javax/net/SocketFactory.createSocket` natives from `be6055605` feeding a
never-really-constructed `java/net/Socket` to real Socket bytecode under
`CRATONVM_REAL_NET_SOCKETS=1`) is now fixed on dev: the RNS registry
drop-filter also drops `javax/net/SocketFactory`, so real
`SocketFactory`/`DefaultSocketFactory` bytecode constructs sockets through
the real `Socket` constructors (`javax/net/ServerSocketFactory` is
deliberately kept — its natives delegate to real constructors and are
layout-correct). Post-fix validation on the Windows suite runner:
`TestCancelledUpload` OK (2 tests) (was 2/2 failing);
`TestHttp2Section_5_1` 23/26 (was 26/26 failing — the 3 remaining are
newly-VISIBLE protocol-behavior residuals, split off to
[http2-section51-maxactivestreams-rst-behavior.md](http2-section51-maxactivestreams-rst-behavior.md));
the DoHead family's `testDoHeadHttp2` parameterizations pass modulo load
flakes, with zero occurrences of any of the three signatures.

Original write-up follows for the record.

**Status:** OPEN. **Severity:** high. **HotSpot:** PASS (fresh-verified).
**Related:** [dohead-jit-heap-corruption-register-invisibility.md](dohead-jit-heap-corruption-register-invisibility.md)
— that doc's "new blocker" section documents the identical signature for
the DoHead family; this doc extends the finding to non-DoHead HTTP/2 test
classes, confirming it's a general `Http2TestBase` connection-handling
issue, not something specific to DoHead's servlet code path.

**2026-07-13:** re-verified live on `TestCancelledUpload` — still
reproduces; a run with debug env vars added surfaced a `socketLock`-is-null
NPE instead of the `String.setOption` NoSuchMethodError below, which
initially looked like GC-timing non-determinism. It is not: this rules out
a shared-vtable-dispatch-bug hypothesis raised while investigating
`largeclienthello-string-size-nosuchmethod.md` (now fixed, see
`docs/internal/tomcat-08-07/largeclienthello-string-size-nosuchmethod-FIXED.md`'s
"Refuted hypothesis" section — that bug was a deterministic, unrelated
`java.util.logging.Logger` field-layout collision), but a **concurrent
2026-07-13 investigation** ([[project_dohead_third_cause_socketfactory_synthetic_20260713]]
in project memory) found the real, fully deterministic root cause: commit
`be6055605` (2026-07-09) added synthetic `javax/net/SocketFactory
.createSocket` natives building a 5-slot synthetic-layout `Socket`, while
`CRATONVM_REAL_NET_SOCKETS=1` (set by this suite runner) drops every
`java/net/Socket` native so real bytecode consumes that synthetic object —
a producer/consumer layout split-brain, reproducible with `--nojit`. Which
of the three faces you see depends on the exact construction path (e.g.
`useAsyncIO`), not GC timing. Fix (drop `javax/net/SocketFactory` natives
under the same real-net-sockets registry filter) was prepared in that
session's worktree but not yet merged — check dev history before
re-investigating.

## Summary

`org.apache.coyote.http2.TestCancelledUpload` and
`org.apache.coyote.http2.TestHttp2Section_5_1` both fail with the exact
same signature already found in the DoHead family:

```
1) testCancelledRequest[0: loop [0], useAsyncIO[false]](org.apache.coyote.http2.TestCancelledUpload)
java.net.SocketException: Socket is closed
	at java.net.SocketException.<init>(SocketException.java:47)
	at java.net.Socket.setSoTimeout(Socket.java:1275)
	at org.apache.coyote.http2.Http2TestBase.openClientConnection(Http2TestBase.java:700)
	at org.apache.coyote.http2.Http2TestBase.openClientConnection(Http2TestBase.java:692)
	at org.apache.coyote.http2.Http2TestBase.http2Connect(Http2TestBase.java:146)
```
```
1) testClientSendOldStream[0: loop [0], useAsyncIO[false]](org.apache.coyote.http2.TestHttp2Section_5_1)
java.net.SocketException: Socket is closed
	at java.net.SocketException.<init>(SocketException.java:47)
	at java.net.Socket.setSoTimeout(Socket.java:1275)
	at org.apache.coyote.http2.Http2TestBase.openClientConnection(Http2TestBase.java:700)
```
`TestCancelledUpload`: 2/2 failures. `TestHttp2Section_5_1`: 26/26
failures (every parameterization). The failure is in shared HTTP/2 test
harness code (`Http2TestBase.openClientConnection`/`http2Connect`), not in
either test class's own logic — the client-side test socket is already
closed by the time `setSoTimeout` is called on it, meaning the connection
setup between test client and embedded Tomcat server is failing before any
class-specific test logic runs.

Combined with the DoHead family (also `Http2TestBase`-based,
`testDoHeadHttp2[N]` parameterizations) and the related
`NoSuchMethodError: java/lang/String.setOption(ILjava/lang/Object;)V`
signature also seen in DoHead's log (a `Socket.setSoTimeout` call
apparently dispatching into a `String` method surface — see the DoHead doc
for the exact trace), this looks like a systemic issue in CratonVM's
`Socket`/`Http2TestBase` connection-setup path affecting most or all
`org.apache.coyote.http2.*` test classes, not an isolated bug.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on, 1200s timeout). Verified via a fresh
same-session HotSpot run: both PASS cleanly on HotSpot.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName http2sockclosed `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.coyote.http2.TestCancelledUpload
# org.apache.coyote.http2.TestHttp2Section_5_1
```

## Recommendation

Given this now spans at least 3 distinct `coyote.http2` test classes plus
the whole DoHead family's `testDoHeadHttp2[N]` parameterizations, treat
this as a priority, broad-impact bug: trace
`Http2TestBase.openClientConnection`/`http2Connect` against CratonVM's
`java.net.Socket`/native socket-channel implementation to find why the
socket is already closed (or dispatching to the wrong class's method
table, per the `String.setOption` signature) by the time
`setSoTimeout` runs. Given the volume of affected classes, a fix here
likely unblocks a large fraction of the currently-failing `coyote.http2.*`
and DoHead test suite at once. Check whether this correlates with a recent
`Socket`/native-io change (something between whatever commit last had a
clean `coyote.http2` baseline and `080e79256`) — `git bisect` against a
narrow HTTP/2-only class list would likely be fast given tests run in
single-digit seconds to low hundreds of seconds.
