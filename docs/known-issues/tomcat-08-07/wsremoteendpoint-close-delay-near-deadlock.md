# TestWsRemoteEndpointImplServerDeadlock — ~19.1s close delay (near-deadlock)

**Status:** OPEN. **Severity:** high (deadlock-adjacent, WebSocket close
path). **HotSpot:** PASS.

## Summary

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
fails
`testTemporaryDeadlockOnClientClose[0: useAsyncIO[false], sendOnContainerThread[false]]`:
```
1) testTemporaryDeadlockOnClientClose[0: useAsyncIO[false], sendOnContainerThread[false]]
   (org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock)
java.lang.AssertionError: Close delay was [19099264200] ns
```
`19099264200` ns ≈ **19.1 seconds**. The test's name and purpose (a
regression test for a *temporary* deadlock on client-close, i.e. the
close-handshake path must not permanently deadlock but IS allowed some
bounded delay) means the test is asserting the close delay stays under some
threshold — CratonVM's actual delay of ~19.1s blows well past whatever that
threshold is, indicating the code path this test guards against (some lock
contention or blocking-wait during WebSocket close-frame handling when
`useAsyncIO=false` and `sendOnContainerThread=false`) still exhibits a
long stall on CratonVM, even if it does eventually complete (hence a
timeout-style AssertionError with a concrete delay value rather than a HANG).

Only the `[0]` parameter combination
(`useAsyncIO=false, sendOnContainerThread=false`) was observed failing in
this pass — other parameter combinations were not confirmed pass/fail
individually and should be checked.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName wsdeadlock `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock
```

## Recommendation

Treat this as higher priority than the other bare-assertion clusters in
this batch — a near-20-second stall on a path explicitly named/tested for
deadlock-avoidance is a strong signal of a real lock-ordering or blocking-
wait issue in CratonVM's WebSocket close-handshake handling under the
`useAsyncIO=false` (blocking I/O) server-thread-driven send path. Cross-
reference against
[vtable/class_manager deadlock](../../../CLAUDE.md) (see memory
`reference_vtable_classmanager_lock_ordering_deadlock`, OPEN VM-core) and
other known lock-ordering issues in this codebase — this may be the same
family or a WebSocket-connector-specific instance. Get a thread dump during
the ~19s stall window to identify exactly what's being waited on.

## 2026-07-09 focused worker evidence

Ran the single JUnit class manually with a unique VM binary:
`target-tomcat0807_ws/release/cratonvm-tomcat0807_ws-close-delay.exe`.
The Tomcat app fixture was not present in this integration worktree, so the
classpath/build output came from the existing local Tomcat fixture at
`C:\craton\CratonVM\apps\tomcat`; probe logs and temp directories were written
under this worktree's `diagnostics\tomcat0807_ws_close_delay`.

Default JIT/real-JDK run:

```text
Time: 112.869
There were 4 failures:
[0: useAsyncIO=false, sendOnContainerThread=false] Close delay was [19098073200] ns
[1: useAsyncIO=false, sendOnContainerThread=true]  Close delay was [19145564900] ns
[2: useAsyncIO=true,  sendOnContainerThread=false] Close delay was [19144322800] ns
[3: useAsyncIO=true,  sendOnContainerThread=true]  Close delay was [19150722000] ns
```

`--nojit` run with the same focused class also failed all four cases:

```text
Time: 111.206
[0] Close delay was [19127170600] ns
[1] Close delay was [19099221700] ns
[2] Close delay was [19097629900] ns
[3] Close delay was [19091414900] ns
```

This rules out a JIT-only stale field/static-value explanation.

Stack dump at 28s captured the stall in the first parameter case. The server
close path is repeatedly in:

```text
org/apache/tomcat/websocket/WsSession.onClose
org/apache/tomcat/websocket/WsSession.sendCloseMessage
org/apache/tomcat/websocket/WsRemoteEndpointImplBase.sendMessageBlockInternal
org/apache/tomcat/websocket/server/WsRemoteEndpointImplServer.acquireMessagePartInProgressSemaphore
```

The matching client-side AIO dispatcher wait-site is:

```text
cratonvm-aio-dispatch
org/apache/tomcat/websocket/WsFrameClient$WsFrameClientCompletionHandler.completed
org/apache/tomcat/websocket/WsFrameClient.processSocketRead
org/apache/tomcat/websocket/WsFrameBase.sendMessageText
org/apache/tomcat/websocket/pojo/PojoMessageHandlerWholeBase.onMessage
org/apache/tomcat/websocket/server/TestWsRemoteEndpointImplServerDeadlock$Bug66508Client.onMessage
```

`Bug66508Client.onMessage()` is waiting on the test's
`clientReceiveLatch.await()`. The test main thread should call
`clientReceiveLatch.countDown()` at `count == 10` (about one second after
`session.close()`), so the current best lead is the latch/monitor wakeup or
the foreign attached AIO dispatcher's wait-site interaction, not the server
`Semaphore` permit itself.

A candidate change that force-dispatched the registered native
`java.util.concurrent.Semaphore` public surface was tested and then reverted:
it passed a focused Rust predicate unit test but did not reduce the Tomcat
delay (`19096918200`..`19638333300` ns). Do not treat Semaphore routing as the
root cause unless new evidence shows otherwise.

Current status: still OPEN. Focus next on
`native-builtins/src/lib.rs::native_cdl_await` /
`native_cdl_count_down`, `vm/src/vm/vm_exec.rs::monitor_wait`, and
`native-io/src/async_socket.rs` handler-form read dispatch on the
`cratonvm-aio-dispatch` thread.

## 2026-07-09 (later session) — root-cause hypothesis strengthened via source reading; repro blocked by 3 separate environment bugs (2 now fixed)

### Hypothesis: bounded blocking-send timeout expiring, not a permanent deadlock

Read the real Tomcat source (`org/apache/tomcat/websocket/{Constants,WsSession,WsRemoteEndpointImplBase}.java`,
`org/apache/tomcat/websocket/server/{WsRemoteEndpointImplServer,TestWsRemoteEndpointImplServerDeadlock}.java`)
rather than guessing. Key findings:

- The test's own comment: *"Send times out after 20s so test should
  complete in less than that."* — `Constants.DEFAULT_BLOCKING_SEND_TIMEOUT
  = 20 * 1000` ms. This is the timeout passed to
  `WsRemoteEndpointImplBase.sendMessageBlockInternal` when the server sends
  its close-frame response, and it bounds
  `WsRemoteEndpointImplServer.acquireMessagePartInProgressSemaphore`'s
  wait/yield loop for the `messagePartInProgress` semaphore.
- `Bug66508Endpoint.serverSession`'s `state` field (polled by the test)
  only flips to `CLOSED` once the server finishes sending its own close
  response, which requires `messagePartInProgress` to become available —
  it's held by the background thread's async `sendText(MSG)` call that got
  stuck because the client wasn't reading.
- The observed ~19.0-19.15s delay is **just under** 20s (short by the
  ~0.85-1s the test's own polling loop takes to reach `count==10` and
  release `clientReceiveLatch`), consistent with the semaphore holder
  *never* completing/releasing within the 20s budget, rather than a
  genuinely permanent deadlock (which the loop's `Thread.yield()`-based
  design is explicitly there to avoid, per the extensive doc comment on
  `acquireMessagePartInProgressSemaphore`) or a quick release.

This means: whatever normally lets the server's stuck async write resume
(and release the semaphore) once the client resumes reading at the ~1s
mark — either the socket write-readiness notification never fires, or the
`clientReceiveLatch.countDown()` signal / the client's resumed read never
actually drains the socket — isn't happening on CratonVM, so the
`acquireMessagePartInProgressSemaphore` loop just burns its full 20s
budget and returns `false`, at which point `sendMessageBlockInternal`
calls `doClose(...)` (marking `state = CLOSED`), matching the observed
"just under 20s" delay precisely. **Not yet empirically confirmed** — see
"Blocked" below — but this is a materially stronger, source-grounded
version of the original close-look hypothesis, and rules out treating this
as a classic AB-BA lock-ordering deadlock.

Ruled out as the cause (tested directly, standalone, not just inferred):
`java.util.concurrent.CountDownLatch` cross-thread wake latency is fine —
a `native_cdl_await`/`native_cdl_count_down` micro-benchmark (waiter
thread parks, main thread sleeps 500ms then `countDown()`s) shows 0ms
delay from `countDown()` to the waiter unblocking, on this exact build.
`native_cdl_await`'s design (10ms-bounded `monitor_wait` loop, re-checking
`cdl_count` every iteration rather than relying purely on
`monitor_notify_all`) is self-healing even if notify were lost. So
`Bug66508Client.onMessage()`'s `clientReceiveLatch.await()` is very
unlikely to be where the ~19s comes from; the leading suspect is now the
**server-side socket write-readiness / async-send-completion path**
(`native-io/src/async_socket.rs`, `native-io/src/nio_selector.rs`, or the
`WsRemoteEndpointImplServer.doWrite`/`onWritePossible` interaction with
whatever backs `SocketWrapperBase.isReadyForWrite()`/write-listener
dispatch under real-JDK NIO), **not** yet directly confirmed via a live
gdb backtrace or targeted tracing during an actual repro run — that's the
next step once the repro is unblocked (see below).

### Blocked: 3 separate, pre-existing environment bugs prevented ever reaching this code path

Attempting the exact repro command from this doc (`JUnitCore
TestWsRemoteEndpointImplServerDeadlock`, real JDK 25, Linux, fresh `dev`
worktree build) no longer reproduces the "~19s close delay" assertion at
all — instead **`Tomcat.start()` itself fails** before the test's own
close-handshake logic is ever exercised. This is a *regression* relative
to the state this doc was originally written in (the 2026-07-07 Linux
confirmation referenced above, `/data/wt-linux-nonpassed1200/...`, shows
zero occurrences of any of the errors below) — something changed on `dev`
between then and 2026-07-09 that exposed (or newly introduced) these
bugs. Root-caused and two of the three fixed this session:

1. **`java.io.File.FS` never set by the native `<clinit>` override for
   `java/io/File`** — FIXED, see
   `docs/internal/fixed-suite-bugs/file-fs-native-clinit-never-set-FIXED.md`.
   Caused Tomcat's `Digester`/`SAXParserFactory` bootstrap (loading
   `mbeans-descriptors.xml`) to NPE repeatedly on
   `FileSystem.isInvalid(File)`, cascading into
   `StandardContext startup failed due to previous errors`. 100%
   reproducible pre-fix, every run.
2. **`java.lang.ThreadGroup` native accessors used a stale/swapped field
   layout** — FIXED, see
   `docs/internal/fixed-suite-bugs/threadgroup-native-field-index-mismatch-FIXED.md`.
   Corrupted every `ThreadGroup` built via the VM's own bootstrap
   (`parent`/`name` swapped), so
   `jdk.internal.misc.InnocuousThread.<clinit>` (triggered by
   `java.lang.ref.Cleaner`, itself triggered very early during
   `StandardServer` init) threw `ClassCastException: String cannot be cast
   to ThreadGroup`, failing `LifecycleException: Failed to initialize
   component [StandardServer[-1]]`. Also 100% reproducible pre-fix.
3. **`EnumSet.of(...)` silently returns a broken/empty set for non-JDK
   enums** (e.g. `jakarta.servlet.DispatcherType`) — **still OPEN**, see
   `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`. This is the
   *current* blocker: with (1) and (2) fixed, Tomcat now gets as far as
   `WsServerContainer`'s constructor (`EnumSet.of(DispatcherType.REQUEST,
   DispatcherType.FORWARD)` for the WebSocket filter mapping), which
   returns an unusable set, and the subsequent for-each NPEs
   (`Iterator.hasNext()` on a null iterator), failing every
   `StandardContext` that registers a websocket endpoint — including this
   test's `Bug66508Config`. Root-cause narrowed to two candidate
   explanations (see that doc) but not fully confirmed or fixed.

**Net effect: the original close-delay bug in this doc has not yet been
re-confirmed, root-caused at the I/O level, or fixed this session** — the
entire session's effort after the initial source-reading went into
unblocking the repro path, which is not yet fully unblocked. The two
fixes above are real, verified, valuable fixes in their own right
(committed to `dev` independently — see their docs for verification
detail) but are not the fix for *this* bug.

### What a future session should do

1. Fix (or work around) `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`
   first — it's the last known blocker preventing
   `TestWsRemoteEndpointImplServerDeadlock` from starting Tomcat at all.
2. Re-run the exact repro from this doc on a fresh worktree build once
   unblocked. Confirm whether the ~19s delay is still present (it may not
   be — none of the three environment bugs above are obviously related to
   the close-handshake path itself, but this doc's original evidence
   predates all of them, so treat the delay's continued presence as
   needing re-confirmation, not an assumption).
3. If still present: attach `gdb -p <pid> -batch -ex 'thread apply all bt'`
   to the CratonVM process during the ~1-19s window (not just at a single
   timeout-triggered snapshot) to see whether the server thread is
   genuinely parked waiting on the socket (native syscall, e.g. `epoll`/
   `poll`) vs. spinning in the `acquireMessagePartInProgressSemaphore`
   yield loop the whole time — this directly distinguishes "write-ready
   notification never fires" from "notification fires but something else
   in the completion chain drops it." Cross-reference with the
   `DEFAULT_BLOCKING_SEND_TIMEOUT` hypothesis above.
4. If the timeout hypothesis holds, the actual fix is almost certainly in
   `native-io/src/async_socket.rs`/`nio_selector.rs` (or wherever
   `SocketWrapperBase.isReadyForWrite()`/the NIO write-listener dispatch
   is backed) — not in `Semaphore`/`CountDownLatch` (already tried and
   ruled out respectively by a prior session and this one).
