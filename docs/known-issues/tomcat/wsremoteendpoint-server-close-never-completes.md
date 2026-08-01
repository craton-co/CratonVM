# `TestWsRemoteEndpointImplServerDeadlock`: the server session never reaches CLOSED

**Status:** OPEN (diagnosed only). **HotSpot:** PASS (4/4).
Found 2026-08-01 while fixing the separate inline-completion deadlock in the
same test (`docs/internal/fixed-suite-bugs/tomcat/
websocket-client-completion-on-app-thread-deadlock-FIXED.md`).

## Symptom

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
fails ~35% of runs with

```
java.lang.AssertionError: Close delay was [19034931192] ns
```

19.03 s is the test's own polling ceiling (190 x 100 ms), i.e. the server
`WsSession.state` never became `CLOSED` at all — not "closed slowly". When it
fails it usually fails 3 of the 4 parameter combinations at once.

Measured 20 runs per binary, interleaved, on an idle box: 3/11 of completed
runs before the inline-completion fix, 7/20 after (z ~= 0.45, p ~= 0.65 —
unchanged by that fix, which addressed a different failure mode in the same
test). Real JDK 25 on the same fixture: 4/4 PASS, so this is a CratonVM defect,
not a fixture artifact.

## What the test expects

The client stops reading (its `@OnMessage` blocks on a latch), so the server's
send loop fills the socket buffers and gives up after its 2 s per-message
`Future.get` timeout. The client then closes the session. Tomcat bug 66508 is
precisely that the server's processing of that close must not have to wait for
the blocked send to time out; the test asserts the server reaches `CLOSED`
within 10 s, and releases the client latch 1 s into polling so the backlog can
drain.

## What was observed

`--stack-dump-on-timeout=15`, caught mid-poll on a failing run:

- `main` — at `testTemporaryDeadlockOnClientClose@249`, i.e. the polling loop
  itself. Healthy; it is genuinely waiting on the server state.
- **225 registered threads, 201 of them parked in
  `AbstractQueuedSynchronizer$ConditionObject.awaitNanos`** — Tomcat's exec pool
  grown to its full `maxThreads=200`, all idle in
  `ThreadPoolExecutor.getTask()`.
- `NioEndpoint$Poller.run` idle, `NioEndpoint.serverSocketAccept` idle.
- No thread anywhere in a WebSocket write, a close handler, or
  `Bug66508Client.onMessage`.

So by the time the delay is observable, nothing is working on the close: the
client latch has already been released, the client is free, the server pool is
idle, and the server session is simply still not `CLOSED`.

Two things to chase, in order:

1. **Why did the pool grow to 200?** A handful of threads should serve this
   test. Reaching `maxThreads` suggests the poller re-dispatched the same
   socket repeatedly, each dispatch taking a fresh thread. Worth confirming
   against HotSpot's thread count on the same run.
2. **Did the client's close frame reach the server, and was it processed?** The
   client close is a Future-form write performed by an AIO worker, so the bytes
   go out independently of completion delivery. Instrument the server-side
   `WsSession` state transitions (`OPEN` -> `CLOSING` -> `CLOSED`) and the
   frame reader to see whether the CLOSE frame arrives.

## Reproduction

`/data/data/wsdead-probes/wsd.sh <exe> <outfile> [dump_after_s] [hard_timeout_s]`
on the Azure host, against the Tomcat fixture at `/data/data/apps/tomcat`
(classpath `.suite/cp-linux-fixed.txt`). Roughly 1 run in 3 fails; classify with

```
grep -q '^OK ('        -> PASS
grep -q 'Close delay was' -> this bug
```

Real-JDK control: same command line with `/home/victor/jdk25/bin/java`.
