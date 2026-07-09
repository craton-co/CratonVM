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
