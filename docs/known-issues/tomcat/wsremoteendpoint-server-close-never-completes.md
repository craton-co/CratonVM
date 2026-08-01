# `TestWsRemoteEndpointImplServerDeadlock`: the server session never reaches CLOSED

**Status:** ROOT-CAUSED 2026-08-01. The defect is not in the WebSocket stack —
it is a JIT miscompilation, filed separately at
`docs/known-issues/jit-direct-call-arg1-clobbered-by-arg0.md`. **HotSpot:** PASS.
Found while fixing the separate inline-completion deadlock in the same test
(`docs/internal/fixed-suite-bugs/tomcat/websocket-client-completion-on-app-thread-deadlock-FIXED.md`).

## Symptom

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
fails ~35% of runs with

```
java.lang.AssertionError: Close delay was [19034931192] ns
```

19.03 s is the test's own polling ceiling (190 x 100 ms), i.e. the server
`WsSession.state` never became `CLOSED` at all. When it fails it fails parameter
combinations 0, 1 and 2 and never 3 — combination 3
(`useAsyncIO=true, sendOnContainerThread=true`) passes for a degenerate reason:
its server session dies immediately with `ClosedChannelException` and closes with
code 1006, well inside the polling window.

## The chain

Established with an instrumented replica of the test (compiled outside the Tomcat
fixture and prepended to the classpath), `tcpdump` on loopback, and
`CRATONVM_DBG_SC_CLOSE=1`. **48 runs: `closedelay` and the count of Tomcat's
"Executor rejected socket" warning agree 1:1, in every single run.**

1. Under CratonVM the connector's exec pool grows from 10 to its full
   `maxThreads=200` about 0.5 s into the test — CratonVM dispatches ~5,600
   socket-processing tasks where HotSpot dispatches 524, so Tomcat's `TaskQueue`
   does exactly what it is designed to do under load and spawns threads to the
   cap. HotSpot's pool never leaves 10. (A throughput difference, not a defect;
   it is the *precondition*, not the cause.)

2. At `poolSize == maxPoolSize` there is a benign race in Tomcat's own executor:
   `TaskQueue.offer` can return `false` (asking for a new thread) just as
   `addWorker` starts refusing, so `execute()` lands in its
   `RejectedExecutionException` recovery path and calls `TaskQueue.force`.
   HotSpot reaches this path too; there, `force` simply queues the task.

3. `TaskQueue.force` throws only for `parent == null || parent.isShutdown()`.
   On CratonVM it throws while the connector is running. Two independent
   captures with a diagnostic shadow of `TaskQueue`:

   ```
   [TASKQ] force-reject p1=@157489 sd1=true p2=@157489 sd2=false sd3=false
           ctl=-536870712 (0xe00000c8) pool=200 max=200 thread=...-Poller
   ```

   `parent` is the same non-null object on both reads; `isShutdown()` answers
   `true` and then `false` microseconds later; `ctl` is `0xe00000c8`, negative,
   so `runStateAtLeast(ctl.get(), SHUTDOWN)` — literally `c >= 0` — must be
   false. A sampler reading `isShutdown()` on the same executor 50 ms either side
   reports `false` throughout.

4. `AbstractEndpoint.processSocket` catches the rejection and Tomcat closes the
   socket at `NioEndpoint$Poller.processKey:1005`:

   ```
   [SC_CLOSE] id=0x60000001 local=127.0.0.1:37115 peer=127.0.0.1:54974
     at NioChannel.close(NioChannel.java:109)
     at NioEndpoint$NioSocketWrapper.doClose(NioEndpoint.java:1483)
     at SocketWrapperBase.close(SocketWrapperBase.java:668)
     at NioEndpoint$Poller.processKey(NioEndpoint.java:1005)
   ```

   This bypasses `WsSession` entirely, which is why the session stays `OPEN`,
   `WsRemoteEndpointImplServer.closed` stays `false`, and no `onError` /
   `onClose` fires.

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

Step 3 is the defect. It reduces to a JIT miscompilation of `f(g(), k)` where the
callee compares its two int arguments — see
`docs/known-issues/jit-direct-call-arg1-clobbered-by-arg0.md`, which carries the
disassembly and a 20-line repro.

## Reproduction

`/data/data/wsdead-probes/probe-run.sh <exe> <outfile>` on the Azure host, with
`PROBE_COMBOS=0` to run only the first parameter combination (~8 s per pass,
~25 s per failure). Roughly 1 run in 3 fails; the rate varies with host load.
Classify on either signature — they are equivalent:

```
grep -c 'Close delay was'            # the assertion
grep -c 'Executor rejected socket'   # the cause, one per failure
```

Real-JDK control: the same command line with `/home/victor/jdk25/bin/java`.
