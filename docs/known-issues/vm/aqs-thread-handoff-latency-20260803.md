# Every AQS-mediated thread handoff is 13-23x slower than HotSpot

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | high — this is a VM-wide primitive, not one library's problem |
| **Discovered** | 2026-08-03, root-causing `TestAsyncMessagesPerformance` SEQ2 |
| **Owns** | the residue of the retired [`websocket-async-send-interframe-latency`](../../internal/fixed-suite-bugs/tomcat/websocket-async-send-interframe-latency-CLOSED-20260803.md) doc |

## The measurement

Quiet host, interleaved, both arm orders, medians of 3. `probes/HandoffLayersProbe.java`
and `probes/ExecDispatchProbe.java`; `dev` @ `a64f3a5b4` plus the invoke-cache
fix in this branch.

| one-way handoff | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `LockSupport.unpark` → `park` returns | 0.7 us | 2.3 us | 3x |
| `Object.notify` → `wait` returns | 2.7 us | 9.7 us | 4x |
| **`Condition.signal` → `await` returns** | **7.4 us** | **96.9 us** | **13x** |
| `LinkedBlockingQueue.put` → `take` returns | 3.2 us | 83.5 us | 26x |
| **`ThreadPoolExecutor.execute` → task entered** | **7.9 us** | **167.1 us** | **21x** |

The shape is the point: the two primitives CratonVM implements directly
(`park`/`unpark`, monitor `wait`/`notify`) are within 3-4x. Everything built on
`AbstractQueuedSynchronizer`'s `ConditionObject` is an order of magnitude worse.
That covers every `BlockingQueue`, every `ThreadPoolExecutor`, every
`ReentrantLock` condition — i.e. essentially all thread handoff in real Java
code.

## Why

`AbstractQueuedSynchronizer.acquire` spins before it parks, doubling its budget
each time it has to park (`spins = postSpins = (byte)((postSpins << 1) | 1)`)
until it saturates around 255 rounds. Each round runs interpreted bytecode plus
a `Thread.onSpinWait()` call and a `tryAcquire` CAS. On HotSpot the whole spin
is ~11 us and usually acquires without parking at all; on CratonVM the same 255
rounds cost hundreds of microseconds, so the spin is pure overhead — the lock
has long been free.

One contributor is now fixed (see the branch this doc lands on): the
invokestatic inline cache had been globally suppressed since the 2026-07-04
`loader_aware_resolution` default flip, so `Thread.onSpinWait()` cost 807 ns per
call instead of 109 ns. That moved `Condition.signal → await` from 118.5 us to
96.9 us — real, and nowhere near enough.

What remains is the general per-call floor. Measured in the same probe:

| | HotSpot | CratonVM |
|---|---|---|
| empty static call, user class | 0.2 ns | 166 ns |
| `Thread.onSpinWait()` (empty JDK static, now intrinsified) | 44 ns | 109 ns |

An interpreted call is ~800x HotSpot's inlined one. Until that closes, ~255
spin rounds cannot cost less than tens of microseconds, and an AQS handoff
cannot approach HotSpot's single-digit microseconds.

## What this blocks

- `org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance.testAsyncTiming`
  — SEQ2 gives a 500 us budget to the gap between two async WebSocket messages.
  The server's completion path (`WsRemoteEndpointImplServer.clearHandler` →
  `socketWrapper.execute(OnResultRunnable)` → `semaphore.release()` → the
  endpoint thread's `semaphore.acquire()` returns → next `sendBinary`) contains
  **two** AQS handoffs, so it cannot fit. Measured: 476-491 breaches of 500,
  against HotSpot's 0-2.
- Anything else whose latency budget is a thread handoff. The
  `SmokeTests` concurrency ceiling and the H2 `INSERT`+`commit` throughput gap
  are plausible relatives; not yet confirmed against this measurement.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\HandoffLayersProbe.java probes\ExecDispatchProbe.java
& "$jdk\bin\java.exe" -cp out HandoffLayersProbe        # HotSpot control
& <cratonvm.exe> --java-home $jdk -cp out HandoffLayersProbe
```

Run the arms **interleaved and in both orders**, on a quiet host. Under
background load every number in the table above roughly triples and the
CratonVM/HotSpot ratio changes — an early pass of this investigation reported
`execute` at 558 us for exactly that reason.

## Next steps

1. The per-call floor is the whole story now. `empty static (user class)` at
   166 ns interpreted is the number to attack; see the compiled-dispatch work in
   `docs/internal/jit-raw-jit-to-jit-shadow-stack-overflow-FIXED-20260731.md`
   for the JIT-side counterpart (992 ns mono / 6027 ns poly `invokevirtual`).
2. `AbstractQueuedSynchronizer.acquire` is not JIT-compiled
   (`CRATONVM_DBG=jit-compiled` shows only its leaf helpers — `signalNext`,
   `ConditionNode.block`, `Node.getAndUnsetStatus`). Whether `acquire` is
   admissible, and what it costs compiled, is unmeasured.
3. `tryAcquire`'s CAS runs once per spin round through
   `Unsafe.compareAndSetInt`. Its per-call cost was never isolated; it is the
   other half of the spin-round budget and may be the larger half now that
   `onSpinWait` is 109 ns.
