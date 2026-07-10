# TestSwallowAbortedUploads — client sees `SocketException` when none expected

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.core.TestSwallowAbortedUploads.testAbortedPOSTOKSwallow`
fails:
```
java.lang.AssertionError: Unlimited upload with swallow enabled generates client exception
  expected null, but was:<java.net.SocketException: SocketException: Connection aborted: write0:
  Программа на вашем хост-компьютере разорвала установленное подключение. (os error 10053)>
```
(Russian OS text = "The program on your host computer aborted an established
connection" — a standard Windows WSAECONNABORTED message, localized.)

The test's premise: when the server aborts reading an oversized/invalid
upload but has "swallow input" enabled, the *client* should NOT see a
connection-reset exception while writing its POST body — the server is
expected to keep the connection alive and drain (swallow) the unwanted
upload bytes rather than abruptly closing the socket. On CratonVM, the
client's write does hit a hard `SocketException` (`os error 10053`,
`WSAECONNABORTED`), meaning the server side closed/reset the connection
instead of swallowing and draining it.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName swallowup `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestSwallowAbortedUploads
```

## Recommendation

Investigate Tomcat's "swallow input" mechanism
(`org.apache.catalina.connector.Request`'s input-swallowing path /
`Http11Processor`'s handling of an aborted request body) under CratonVM —
check whether CratonVM's socket/connector layer is closing the connection
too eagerly (e.g. on an unhandled exception during body read) instead of
continuing to read-and-discard bytes per Tomcat's intended swallow
semantics. Likely a real-socket / NIO-connector behavioral gap rather than a
pure VM correctness bug — worth checking against other socket-abort-related
findings from this investigation (`TestAccessLogValve`/`TestRewriteValve`'s
`-1` symptom, if related).

## 2026-07-09 worker isolation

Candidate root cause confirmed at the native socket contract level:
`native-io/src/socket_channel.rs::sc_close` unconditionally called
`TcpStream::shutdown(Shutdown::Both)` before dropping a Tomcat NIO
`SocketChannel`. On Windows, closing the read side while the peer is still
uploading request-body bytes is abortive/RST-prone and matches the client-side
`WSAECONNABORTED` (`os error 10053`) observed here.

Candidate fix in branch `codex/fix-tomcat0807-http-close-20260709-001`:
`sc_close` now sends only the write-side FIN and starts a bounded background
drain on a cloned stream (`lingering_channel_close`) so a client can finish
writing already-in-flight upload bytes without receiving a reset. This preserves
the earlier selector-wakeup requirement (the peer still sees EOF) without using
`Shutdown::Both`.

Focused proof run:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-tomcat0807-http-private-20260709-001'
cargo test -p cratonvm-native-io socket_channel::tests -- --nocapture
# 7 passed, including tomcat0807_http_lingering_channel_close_drains_peer_upload
```

Connector smoke run:

```powershell
# Unique binary:
# C:\craton\target-tomcat0807-http-private-20260709-001\debug\cratonvm-tomcat0807-http-20260709-001.exe
# Unique probe class:
# C:\craton\smoke-cache-tomcat0807-http-20260709-001\fixture\Tomcat0807HttpSmoke.java
# Result:
# TOMCAT0807_HTTP_BODY=TOMCAT0807_HTTP_SMOKE_OK
```

The full `apps/tomcat-suite-runner` checkout is not present in this worktree,
so `org.apache.catalina.core.TestSwallowAbortedUploads` itself still needs an
isolated rerun before this note can be archived.

## 2026-07-09 Linux re-verify: original symptom confirmed fixed; found 2 more
## blockers (both now resolved) and 1 new, still-OPEN blocker

Re-verified on the Azure Linux build host against the real Tomcat fixture
(`/data/data/apps/tomcat`, real JDK 25, `org.junit.runner.JUnitCore
org.apache.catalina.core.TestSwallowAbortedUploads`, all 10 test methods).

**Original `WSAECONNABORTED` symptom: confirmed FIXED.** `sc_close`'s
`lingering_channel_close` (described above) landed on `dev` via commit
`27c3e9ac` ("fix(tomcat): unblock HttpServlet/DoHead family...",
2026-06-29) — well before this re-verify. `native-io/src/socket_channel.rs`
on current `dev` no longer calls `Shutdown::Both` unconditionally. This part
of the bug is done; do not re-investigate the socket-close path.

**New blocker #1 (found + fixed elsewhere): `ScheduledThreadPoolExecutor.getQueue()`
NPE blocked the test from even starting.** Running the class end-to-end (not
just the socket-close path) surfaced a *different* bug blocking every one of
the 10 methods before any request was even sent:
```
org.apache.catalina.LifecycleException: Failed to start component [StandardEngine[Tomcat]]
Caused by: java.lang.NullPointerException: Cannot invoke "java.util.concurrent.BlockingQueue.add(Object)" because
  the return value of "java.util.concurrent.ThreadPoolExecutor.getQueue()" is null
	at java.util.concurrent.ScheduledThreadPoolExecutor.delayedExecute(ScheduledThreadPoolExecutor.java:347)
	at java.util.concurrent.ScheduledThreadPoolExecutor.scheduleWithFixedDelay(ScheduledThreadPoolExecutor.java:667)
	at org.apache.tomcat.util.threads.ScheduledThreadPoolExecutor.scheduleWithFixedDelay(ScheduledThreadPoolExecutor.java:134)
	at org.apache.catalina.core.ContainerBase.startInternal(ContainerBase.java:764)
```
Root cause: `StandardServer.reconfigureUtilityExecutor()` builds Tomcat's
shared "Catalina-utility-" pool via direct
`new java.util.concurrent.ScheduledThreadPoolExecutor(threads, threadFactory)`
(the real JDK class — Tomcat's own `org.apache.tomcat.util.threads.ScheduledThreadPoolExecutor`
is a thin wrapper delegating to it, not a subclass). CratonVM had **three**
separate, redundant native `<init>` overrides for
`java/util/concurrent/ScheduledThreadPoolExecutor` (`native-builtins/src/phases_early.rs::register_scheduled_executor_natives`,
a real-JDK-mode inline duplicate in `vm/src/vm/vm_init.rs`, and a third,
better one — `native-builtins/src/phases_late.rs::register_p63_scheduled_executor`
— that is only reachable from the synthetic/fake-JDK bootstrap path and
never gets registered in real-JDK mode at all). The two natives that *do*
run in real-JDK mode only ever wrote two legacy synthetic slots
(`corePoolSize`, `shutdown`) and never invoked the real `ThreadPoolExecutor`
constructor chain, so the real, inherited `workQueue` field was left `null`
— exactly the "synthetic native shadows real bytecode" pattern behind the
`File.FS`/`ThreadGroup` bugs (see
`docs/internal/fixed-suite-bugs/file-fs-native-clinit-never-set-FIXED.md` and
`.../threadgroup-native-field-index-mismatch-FIXED.md`).

This was found and fixed **concurrently by a different session** (working the
`EnumSet.of()`/`allOf()` websocket-close bug, branch
`codex/fix-tomcat-wsclose-enumset-20260709-223225`), which dropped the whole
`ScheduledThreadPoolExecutor`/`Executors.newScheduledThreadPool`/`newSingleThreadScheduledExecutor`
native surface in real-JDK mode (via `NativeMethodRegistry`'s existing
`drop_real_layout_synthetic` mechanism — the same one already used for
`java/util/StringJoiner` and, in their change, `java/util/EnumSet`) so the
real JDK bytecode constructs and drives `ScheduledThreadPoolExecutor`
end-to-end. That fix was **independently verified in this session**, both
directly on their (uncommitted, at verification time) worktree and by
re-applying their diff onto a fresh `origin/dev` tip in a disposable
worktree:
- Standalone repro (`STPEProbe.java`: `new ScheduledThreadPoolExecutor(1, tf)`,
  `getQueue()`, `scheduleWithFixedDelay(...)`, `shutdown()`) — passes clean,
  `queue=[]` (non-null) instead of `queue=null`.
- Full `TestSwallowAbortedUploads` run: `getQueue()` NPE occurrences dropped
  from present-in-every-method to **0/10** methods.

**New blocker #2 (found, already fixed elsewhere by the time of this
re-verify): `ByteBuffer.put(int, byte)` `AbstractMethodError`.** Past the
`getQueue()` blocker, `NioEndpoint`'s socket read hit
`AbstractMethodError: method java/nio/ByteBuffer.put(IB)Ljava/nio/ByteBuffer;
has no Code attribute`. This turned out to be the same family as (and
plausibly the identical live-dispatch-site fix for)
`docs/internal/tomcat-08-07/bytebuffer-address-unset-aioobe.md`, fixed on
`dev` via commit `1f1b5d28`/merge `1d7134d0` ("Fix ByteBuffer heap address
initialization", 2026-07-09) — landed independently, unrelated to this
investigation. Re-verified with a worktree built from a fresh `origin/dev`
tip (which includes that fix) plus the `ScheduledThreadPoolExecutor` diff
above: `AbstractMethodError` occurrences also dropped to **0/10** methods.

**New blocker #3 — STILL OPEN, now the sole blocker.** With both of the
above fixed (verified together in one combined build off a fresh `dev` tip),
`TestSwallowAbortedUploads` still fails **100% (10/10 methods, `Tests run:
10, Failures: 20`)** with a new symptom, now hit on every single method:
```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor
  (java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null)
```
Not yet root-caused. Notes for the next session:
- `org.apache.tomcat.util.net.SocketWrapperBase` declares
  `private final ReentrantLock lock = new ReentrantLock();`
  (`SocketWrapperBase.java:66`) — the only `ReentrantLock lock` field found
  in the reachable HTTP (non-websocket) request path via
  `grep -rl 'ReentrantLock lock' java/`.
- `javap -p -c` on the real compiled `SocketWrapperBase.class` confirms the
  constructor bytecode looks correct in isolation: `new ReentrantLock()` +
  `putfield lock` at bci 4–12, immediately after `invokespecial
  Object.<init>` at bci 0. `getLock()` is a plain `getfield` + `areturn`.
  No native `<init>` override is registered anywhere in the tree for
  `SocketWrapperBase`, `NioSocketWrapper`, or `ReentrantLock` that would
  explain a skipped/shadowed write (confirmed via
  `grep -rn 'SocketWrapperBase\|NioSocketWrapper'` across
  `native-builtins/src/*.rs` and `vm/src/vm/vm_init.rs` — the only hit is an
  unrelated TLS comment).
- Two untested hypotheses: (a) a GC/moving-object stale-field-write —
  `SocketWrapperBase` is constructed on the Poller/Acceptor thread but the
  failing `.lock()` call happens later, on a worker thread pulled from the
  executor pool, which is exactly the cross-thread-visibility shape of
  several already-fixed bugs in this codebase (see
  `docs/internal/fixed-suite-bugs/*stale-local*`,
  `*cat2-local-store-upper-half-leak*`); (b) a JIT miscompilation of the
  field-initializer store in `SocketWrapperBase.<init>` (interpreter-vs-JIT
  divergence). Neither has been tested — no repro has been isolated below
  the full Tomcat fixture yet; that's the recommended next step (a minimal
  `NioEndpoint`-driven socket accept/read probe, bypassing the rest of
  Tomcat, to confirm/rule out cross-thread visibility vs. JIT).

**Status stays OPEN.** Do not retire this doc until blocker #3 is fixed and
the full class passes clean. Verification commands used throughout (adjust
the binary path):
```bash
<EXE> --java-home /home/victor/jdk25 -Xmx2g -cp "$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.core.TestSwallowAbortedUploads
```

## 2026-07-10 correction after WebSocket close-delay work

The `lock is null` blocker above is no longer current. A focused probe during
`TestWsRemoteEndpointImplServerDeadlock` showed the misleading `lock` message
came from `java.util.concurrent.LinkedBlockingDeque.clear()` on Tomcat's
WebSocket `WriteBuffer`, not from `SocketWrapperBase.lock` itself; direct
`SocketWrapperBase` construction/read probes kept its `lock` field non-null.

This branch drops the synthetic `LinkedBlockingDeque` fallback surface in
real-JDK mode so the real JDK constructor initializes `lock`, `notEmpty`,
`notFull`, and the linked-node fields. Re-running this class with the final
WebSocket-close binary and a temporary logging basedir no longer shows the
`NioEndpoint ... ReentrantLock.lock() because "lock" is null` processor error.
The class now reaches request/response assertions:

```text
Tests run: 10, Failures: 6
1) testAbortedPOSTOKSwallow
2) testAbortedUploadLimitedNoSwallow
3) testChunkedPUTLimit
4) testAbortedPOST413Swallow
5) testAbortedPOST413NoSwallow
6) testAbortedPOSTOKNoSwallow
```

Status remains OPEN, but the next investigation should start from those six
behavioral assertion failures, not from the old `lock is null` blocker.
