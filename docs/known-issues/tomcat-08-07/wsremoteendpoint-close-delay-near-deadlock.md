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
