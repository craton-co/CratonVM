# `TestWsRemoteEndpointImplServerDeadlock`: the server session never reaches CLOSED

**Status:** cause ROOT-CAUSED and FIXED 2026-08-01; **end-to-end reverification
still owed** (see the last section). The defect was never in the WebSocket stack
— it was a JIT miscompilation, written up at
`docs/internal/fixed-suite-bugs/jit-direct-call-arg1-clobbered-by-arg0-FIXED.md`.
**HotSpot:** PASS. Found while fixing the separate inline-completion deadlock in
the same test
(`docs/internal/fixed-suite-bugs/tomcat/websocket-client-completion-on-app-thread-deadlock-FIXED.md`).

## Symptom

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
failed ~35% of runs with

```
java.lang.AssertionError: Close delay was [19034931192] ns
```

19.03 s is the test's own polling ceiling (190 x 100 ms), i.e. the server
`WsSession.state` never became `CLOSED` at all. When it failed it failed
parameter combinations 0, 1 and 2 and never 3 — combination 3
(`useAsyncIO=true, sendOnContainerThread=true`) passes for a degenerate reason:
its server session dies immediately with `ClosedChannelException` and closes with
code 1006, well inside the polling window.

## The chain

Established with an instrumented replica of the test (compiled outside the Tomcat
fixture and prepended to the classpath), `tcpdump` on loopback, and
`CRATONVM_DBG_SC_CLOSE=1`. **48 runs: the assertion and Tomcat's "Executor
rejected socket" warning agree 1:1, in every single run.**

1. Under CratonVM the connector's exec pool grows from 10 to its full
   `maxThreads=200` about 0.5 s in — CratonVM dispatches ~5,600
   socket-processing tasks where HotSpot dispatches 524, so Tomcat's `TaskQueue`
   does what it is designed to do under load and spawns threads to the cap.
   HotSpot's pool never leaves 10. Precondition, not cause — but the ~10x task
   amplification is its own open throughput question.

2. At `poolSize == maxPoolSize` there is a benign race in Tomcat's own executor:
   `TaskQueue.offer` can return `false` (asking for a new thread) just as
   `addWorker` starts refusing, so `execute()` lands in its
   `RejectedExecutionException` recovery path and calls `TaskQueue.force`.
   HotSpot reaches this path too; there, `force` simply queues the task.

3. `TaskQueue.force` throws only for `parent == null || parent.isShutdown()`.
   On CratonVM it threw while the connector was running. Two independent captures
   with a diagnostic shadow of `TaskQueue`:

   ```
   [TASKQ] force-reject p1=@157489 sd1=true p2=@157489 sd2=false sd3=false
           ctl=-536870712 (0xe00000c8) pool=200 max=200 thread=...-Poller
   ```

   Same non-null `parent` on both reads; `isShutdown()` answered `true` then
   `false` microseconds later; `ctl` was `0xe00000c8`, negative, so
   `runStateAtLeast(ctl.get(), SHUTDOWN)` — literally `c >= 0` — must be false.
   **This is the defect**, and it is the JIT bug linked above:
   `isShutdown()` is `f(g(), k)` with a callee that compares its two arguments,
   and the direct-call edge was passing arg0 in both slots.

4. `AbstractEndpoint.processSocket` catches the rejection and Tomcat closes the
   socket at `NioEndpoint$Poller.processKey:1005`:

   ```
   [SC_CLOSE] id=0x60000001 local=127.0.0.1:37115 peer=127.0.0.1:54974
     at NioChannel.close(NioChannel.java:109)
     at NioEndpoint$NioSocketWrapper.doClose(NioEndpoint.java:1483)
     at SocketWrapperBase.close(SocketWrapperBase.java:668)
     at NioEndpoint$Poller.processKey(NioEndpoint.java:1005)
   ```

   This bypasses `WsSession` entirely, which is why the session stayed `OPEN`,
   `WsRemoteEndpointImplServer.closed` stayed `false`, and no `onError` /
   `onClose` fired.

5. The close emits nothing on the wire: the server has ~2.6 MB queued with the
   client's receive window at zero, so the FIN cannot be sent. 1.6 s later the
   client's close frame arrives at a socket whose application has closed, and the
   kernel answers with a bare RST:

   ```
   819.179148  client > server: Flags [P.], seq 168:176     # the 8-byte close frame
   819.179229  server > client: Flags [R], seq ..., win 0   # 81 us later
   ```

   A passing run instead shows `server > client: Flags [.], ack 176` and the
   session moves `OPEN -> CLOSING -> CLOSED`.

6. The client's next read gets `ECONNRESET`, it stops draining at ~12 messages,
   and the test polls out at 19.03 s with the server session still `OPEN`.

## What is verified, and what is not

Step 3 is fixed and directly measured. `TaskQueue.force` is public, so it can be
driven on a running executor with no dependence on host load
(`probes/ForceProbe.java`, needs the Tomcat jar on the classpath):

| | `force()` calls | "Executor not running" rejections |
|---|---|---|
| HotSpot | 47,786,000 | 0 |
| CratonVM before | 1,681,000 | **1,680,487** (99.97%) |
| CratonVM after | 194,000 | **0** |

**Not yet verified end to end.** The test's own failure rate swings hard with
host load — 5-in-6 during a busy window, then 0-in-14 on both arms of an
interleaved A/B two hours later on the same binaries. Capping `maxThreads` to 20
and to 10 did not restore the failing regime either. So the post-fix runs
(4-combo test PASS 3/3 and 4/4, combo-0 PASS 14/14) are consistent with the fix
but carry little weight on their own: the pre-fix binary passed just as often in
that window.

**To close this doc**, re-run it during a genuinely loaded window and confirm
zero `Executor rejected socket` warnings across ~20 runs where the pre-fix binary
still produces them.

## Reproduction

`/data/data/wsdead-probes/probe-run.sh <exe> <outfile>` on the Azure host, with
`PROBE_COMBOS=0` to run only the first parameter combination (~8 s pass, ~25 s
fail), and `PROBE_MAXTHREADS=<n>` to cap the connector pool. Classify on either
signature — they are equivalent:

```
grep -c 'Close delay was'            # the assertion
grep -c 'Executor rejected socket'   # the cause, one per failure
```

Real-JDK control: the same command line with `/home/victor/jdk25/bin/java`.
