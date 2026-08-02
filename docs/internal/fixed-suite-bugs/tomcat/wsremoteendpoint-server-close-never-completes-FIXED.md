# `TestWsRemoteEndpointImplServerDeadlock`: the server session never reaches CLOSED

**Status:** CLOSED 2026-08-01. Two defects, both fixed and both reverified
end to end. **HotSpot:** PASS.

| | |
|---|---|
| Cause | direct-call argument clobber in the x64 JIT — `5fa4cdb6fb`, write-up at [`../jit-direct-call-arg1-clobbered-by-arg0-FIXED.md`](../jit-direct-call-arg1-clobbered-by-arg0-FIXED.md) |
| Residual (the "10x task amplification") | selector safety-net probe reporting `OP_WRITE` it never checked — fixed in this branch |
| Neighbour | the separate inline-completion deadlock in the same test, [`websocket-client-completion-on-app-thread-deadlock-FIXED.md`](websocket-client-completion-on-app-thread-deadlock-FIXED.md) |

## Symptom

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
failed ~35% of runs with

```
java.lang.AssertionError: Close delay was [19034931192] ns
```

19.03 s is the test's own polling ceiling (190 x 100 ms), i.e. the server
`WsSession.state` never became `CLOSED` at all. It failed parameter
combinations 0, 1 and 2 and never 3 — combination 3 (`useAsyncIO=true,
sendOnContainerThread=true`) passes for a degenerate reason: its server session
dies immediately with `ClosedChannelException` and closes with code 1006, well
inside the polling window.

## The chain, corrected

The original write-up had step 1 as a *precondition* — "CratonVM dispatches
~5,600 socket-processing tasks where HotSpot dispatches 524, so Tomcat's
`TaskQueue` does what it is designed to do under load and spawns threads to the
cap". **That was wrong, and it was wrong in the useful direction: the pool
growth is not a consequence of load at all, it is the same JIT bug one level
up.** Two measurements settle it (both below, in full):

* With the JIT fix reverted, the pool reaches `maxThreads=200` on **20 of 20**
  runs while dispatching *fewer* tasks than HotSpot (446–1135 vs 524).
* With the JIT fix in and the selector defect still present, the pool stays at
  **10 of 10** on 20 of 20 runs while dispatching up to **3,678** tasks.

Task volume does not grow the pool, and the pool does not need task volume to
grow. They are two independent defects that happened to meet in this test.

1. **`TaskQueue.offer` was told the pool had zero threads.** It sizes the pool
   with `ThreadPoolExecutor.getPoolSizeNoLock()`:

   ```java
   if (parent.getPoolSizeNoLock() == parent.getMaximumPoolSize()) return super.offer(o);
   if (parent.getSubmittedCount() <= parent.getPoolSizeNoLock())  return super.offer(o);
   if (parent.getPoolSizeNoLock() <  parent.getMaximumPoolSize()) return false;  // grow
   ```

   and that accessor is `runStateAtLeast(ctl.get(), TIDYING) ? 0 : workers.size()`
   — `f(g(), k)` with a two-argument callee comparing its arguments, over a
   `ctl` that is mutated by every worker create/destroy. Exactly the shape the
   direct-call edge broke: the callee got arg0 in both slots, `c >= s` evaluated
   `c >= c` = **true**, and the method answered **0** for a live pool. A 0 sends
   `offer()` down its grow branch on *every* submission, so the pool runs
   straight to `maxThreads`. HotSpot's stays at 10.

2. At `poolSize == maxPoolSize` there is a benign race in Tomcat's own
   executor: `TaskQueue.offer` can return `false` (asking for a new thread) just
   as `addWorker` starts refusing, so `execute()` lands in its
   `RejectedExecutionException` recovery path and calls `TaskQueue.force`.
   HotSpot reaches this path too; there, `force` simply queues the task.

3. `TaskQueue.force` throws only for `parent == null || parent.isShutdown()`.
   On CratonVM it threw while the connector was running. Two independent
   captures with a diagnostic shadow of `TaskQueue`:

   ```
   [TASKQ] force-reject p1=@157489 sd1=true p2=@157489 sd2=false sd3=false
           ctl=-536870712 (0xe00000c8) pool=200 max=200 thread=...-Poller
   ```

   Same non-null `parent` on both reads; `isShutdown()` answered `true` then
   `false` microseconds later; `ctl` was `0xe00000c8`, negative, so
   `runStateAtLeast(ctl.get(), SHUTDOWN)` — literally `c >= 0` — must be false.
   `isShutdown()` is the same broken shape as step 1, just on a colder path:
   it is only called from `force()`, so it needs a much longer run to be
   compiled at all. **That is why the assertion failed ~35% of the time while
   the pool saturation was deterministic.**

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
   client's close frame arrives at a socket whose application has closed, and
   the kernel answers with a bare RST:

   ```
   819.179148  client > server: Flags [P.], seq 168:176     # the 8-byte close frame
   819.179229  server > client: Flags [R], seq ..., win 0   # 81 us later
   ```

   A passing run instead shows `server > client: Flags [.], ack 176` and the
   session moves `OPEN -> CLOSING -> CLOSED`.

6. The client's next read gets `ECONNRESET`, it stops draining at ~12 messages,
   and the test polls out at 19.03 s with the server session still `OPEN`.

## The second defect: the selector invented write readiness

`kernel_select_linux` runs a probe pass after `epoll_wait` over interest bits
epoll did not report, so a readiness edge missed around a register/interest
change cannot park a reactor. `probe_handle` answered that probe for `OP_WRITE`
with

```rust
if interest & OP_WRITE != 0 { ready |= OP_WRITE; }
```

— no syscall, no socket. Every `select()` cycle therefore reported a connection
whose **send buffer was full** as ready to write. Tomcat's Poller did exactly
what that means: `unreg` the write bit and dispatch a `SocketProcessor`, which
wrote zero bytes, re-armed `OP_WRITE`, and handed the next cycle the same false
positive. The `OP_READ` arm of the same function does a real `peek()`; only
`OP_WRITE` was invented.

Fixed by `os_handle_writable` — a zero-timeout one-fd `poll(2)` (`WSAPoll` on
Windows), with `POLLERR`/`POLLHUP` counting as writable so a failed socket still
surfaces its error through the next `write()` rather than parking. The UDP arm
gets the same check. The safety net keeps its purpose; it just no longer
invents readiness.

## Evidence

### 1. The pool saturation, end to end — 20 interleaved rounds

Both arms are `origin/dev@492ddedd2`; the `pre` arm has **only**
`jit/src/x64.rs` reverted to its pre-`5fa4cdb6fb` state. `PROBE_COMBOS=0`,
alternating order each round, host load 27–173 throughout.

| arm | runs | max pool size reached | tasks dispatched | `Close delay was` | `Executor rejected socket` |
|---|---|---|---|---|---|
| pre (fix reverted) | 20 | **200 / 200** | 446–1135 | 0 | 0 |
| post (dev) | 20 | **10 / 10** | 597–3404 | 0 | 0 |
| HotSpot | 5 | 10 | 524 (521 sends) | 0 | 0 |

40/40 clean separation on the pool, and note the direction of the task counts:
the arm that saturates the pool dispatches *fewer* tasks than the arm that does
not.

The 19 s assertion itself did not reproduce on either arm in this window — its
rate swings from 5-in-6 to 0-in-20 with host load, which is what left the
original write-up unable to close. Step 1 replaces it as the discriminator: it
is the same chain, one link earlier, and it is deterministic.

### 2. Both broken predicates, measured directly

`TaskQueue.force` and `getPoolSizeNoLock` are both reachable from Java on a
running executor, with no pool-size race and no host-load dependence.

`probes/ForceProbe.java` — step 3's predicate:

| | `force()` calls | "Executor not running" rejections |
|---|---|---|
| HotSpot | 47,786,000 | 0 |
| CratonVM before | 1,681,000 | **1,680,487** (99.97%) |
| CratonVM after | 194,000 | **0** |

`probes/PoolSizeNoLockProbe.java` — step 1's predicate, `getPoolSizeNoLock()`
answering 0 while the locked `getPoolSize()` is non-zero (both evaluate the same
expression; the locked one stays interpreted, so it is the control):

| | calls | answered 0 for a live pool |
|---|---|---|
| HotSpot | 12,435,934 | 0 |
| CratonVM before | 338 | **233** (69%) |
| CratonVM after | 478 | **0** |

That probe also carries a second, **inert** oracle — `offer()` returning false at
`poolSize == maxPoolSize`. It never fires, because when `getPoolSizeNoLock()`
answers 0 the *second* branch (`getSubmittedCount() <= 0`) is frequently true as
well and `offer()` returns true anyway. It is documented in the probe so a
future reader does not mistake its 0 for an elimination.

### 3. The task amplification, before and after the selector fix

Connector executor task count vs WebSocket messages actually sent, interleaved,
5 rounds each. The server blasts 8 KB text frames at a client that has stopped
draining, so one dispatch per blocked write is the correct answer.

| | tasks/sends per round | ratio |
|---|---|---|
| HotSpot | 524/521, five times identically | **1.01** every run |
| CratonVM, selector defect present | 3678/333 · 1049/333 · 2982/335 · 3014/335 · 909/335 | 2.71 – **11.05** |
| CratonVM, selector fixed | 337/334 · 336/334 · 335/333 · 336/333 · 336/333 | **1.01** every run |

The spread on the middle row is the tell: the count is however many poller
iterations fit in the window, not anything about the workload. Under `strace`,
which slows the poller, the same binary drops to 368/333 — ratio 1.10.

### 4. Regression check

WebSocket cluster, `wsdead-probes/wscluster2.sh`, selector-fix binary, 2 reps,
against a HotSpot control in the same window:

| class | HotSpot | CratonVM |
|---|---|---|
| **TestWsRemoteEndpointImplServerDeadlock** (this doc, all 4 combos) | PASS (4) | **PASS (4), 2/2 reps** |
| TestWsPingPongMessages | PASS (1) | PASS (1) |
| TestEncodingDecoding | PASS (6) | PASS (6) |
| TestWsSessionSuspendResume | PASS (2) | PASS (2) |
| TestAsyncMessagesPerformance | PASS (1) | **FAIL** — see below |
| ~~TestWsRemoteEndpointImplClient~~ | *no such class* | *no such class* |

`cargo test -p cratonvm-jit`: 92 passed, 0 failed. (The `E0061` at `x64.rs`
that blocked this test build when `5fa4cdb6fb` landed is gone on current dev.)
`cargo test -p cratonvm-native-io`: 376 passed, 1 failed —
`async_socket::tests::audit_failed_delivery_is_remapped_and_releases_roots`,
confirmed failing identically on the unmodified base commit.

## Handed over, not closed

**`TestAsyncMessagesPerformance` is a separate, still-open latency gap.** It is
not part of this chain and neither fix closes it; it is written up on its own at
[`../../../known-issues/tomcat/websocket-async-send-interframe-latency-20260801.md`](../../../known-issues/tomcat/websocket-async-send-interframe-latency-20260801.md).
Interleaved 4 rounds per arm: the failing budget is `SEQ2` (gap between a 16 KB
message and the 4 KB message after it, 0.5 ms, 100 breaches of 500 allowed) and
it reads 443/460/420/426 without the selector fix, 428/435/373/409 with it,
against HotSpot's 65/32/21/0. The selector fix helps `SEQ1` and is neutral on
`SEQ2`. Thread-handoff latency was measured and ruled out
(`probes/HandoffLatencyProbe.java`: 8.5 us median against a 500 us budget), so
the cost is inside the WebSocket send path.

**`TestWsRemoteEndpointImplClient` was never a failure.** The original
validation script listed a class that does not exist in the fixture (the real
one is `org.apache.tomcat.websocket.TestWsRemoteEndpoint`), so JUnit's
`ClassNotFoundException` was being recorded as `FAIL[Tests run: 1, Failures: 1]`
— on HotSpot too. `wscluster2.sh` drops it.

## Reproduction

`/data/data/wsdead-probes/probe-run.sh <exe> <outfile>` on the Azure host, with
`PROBE_COMBOS=0` to run only the first parameter combination (~8 s pass, ~25 s
fail), and `PROBE_MAXTHREADS=<n>` to cap the connector pool. Real-JDK control:
the same command line with `/home/victor/jdk25/bin/java`. Classify on:

```
grep -c 'Close delay was'            # the assertion — flaky, load-dependent
grep -c 'Executor rejected socket'   # step 3 — one per assertion failure
grep -o 'pool=[0-9]*' | sort -n | tail -1   # step 1 — deterministic, use this
```

`ab-e2e.sh <rounds>` runs the interleaved pre/post A/B above; `tasks-ab.sh
<rounds> <exeA> <tagA> <exeB> <tagB>` runs the task-ratio comparison;
`wscluster2.sh <exe|.../java> <tag> [reps]` runs the cluster.

**Harness trap.** `probe-run.sh` and `wscluster.sh` both exported the retired
per-flag spelling — `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`. A current binary
prints `[cratonvm] 4 per-flag variable(s) set directly` on line 1 and then runs
with **none** of them, which is a different VM configuration than every number
in the original write-up was taken under. The grouped spelling is
`CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog
CRATONVM_JIT=rootsnap-cache`; both scripts are fixed and `wscluster2.sh` is the
one to use.
