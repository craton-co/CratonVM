# `TestAsyncMessagesPerformance`: the gap between two async WebSocket messages

| | |
|---|---|
| **Status** | ✅ **CLOSED 2026-08-03** — root cause found and characterised; one real defect fixed; the residue is re-homed |
| **Test** | still FAILS (SEQ2), and is expected to until the residue doc closes |
| **HotSpot** | PASS 3/3 (SEQ2 = 1, 2, 0 of 500) |
| **CratonVM** | FAIL 3/3 (SEQ2 = 476, 486, 481 of 500; before the fix, 491, 489, 483) |
| **Residue owned by** | [`docs/known-issues/vm/aqs-thread-handoff-latency-20260803.md`](../../../known-issues/vm/aqs-thread-handoff-latency-20260803.md) |

This doc opened with "**root cause not found**" and three Next Steps. All three
are answered below. It is closed on the same basis as
[29](29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md) and
[32](32-doc04-residual-perf-assertions-CLOSED.md): the investigation is
finished, the defects it turned up are fixed, and what is left is a VM-wide
throughput problem that now has a document of its own.

## Next Step 1 — "split SEQ2 into its two halves"

Answered without needing the instrumentation the doc proposed. SEQ2 is the
completion side, and the mechanism is not in the WebSocket code at all.

`WsRemoteEndpointImplServer.clearHandler(null, /* useDispatch */ true)` does not
call the `SendHandler` inline — it does
`socketWrapper.execute(new OnResultRunnable(...))`, handing the callback to
Tomcat's container `ThreadPoolExecutor`. Only when that runnable runs on a pool
thread does `semaphore.release()` happen, which is what unblocks the endpoint
thread to issue the 4 KB message. So SEQ2 contains a full
`ThreadPoolExecutor.execute() → task entered` dispatch, plus the
`release()`/`acquire()` handoff after it — **two** AQS-mediated handoffs inside
a 500 us budget.

Measured (quiet host, interleaved, medians of 3 — `probes/ExecDispatchProbe.java`):

| | HotSpot | CratonVM |
|---|---|---|
| `ThreadPoolExecutor.execute()` → task entered | 7.9 us | 167 us |
| `Condition.signal` → `await` returns | 7.4 us | 96.9 us |

Two of those plus the write path does not fit in 500 us, which is exactly what
the test reports: 476-491 breaches of 500.

## Next Step 3 — "the p99 158 us on Semaphore is worth its own look"

It was the right instinct pointed one layer too low. The doc's probe measured
`Semaphore`, `LockSupport` and `Object.wait` and found 2.7-10x gaps — real, but
two orders of magnitude short of the 0.5-2.7 ms it was trying to explain, which
is why the doc concluded "the cost is inside the WebSocket send path itself".
None of those three is the primitive on the path. Layering the measurement
properly shows where the cliff is:

| one-way handoff | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `LockSupport.unpark` → `park` | 0.7 us | 2.3 us | 3x |
| `Object.notify` → `wait` | 2.7 us | 9.7 us | 4x |
| **`Condition.signal` → `await`** | **7.4 us** | **96.9 us** | **13x** |
| `LinkedBlockingQueue.put` → `take` | 3.2 us | 83.5 us | 26x |

Everything CratonVM implements directly is within 3-4x. Everything built on
`AbstractQueuedSynchronizer`'s `ConditionObject` is an order of magnitude worse
— and that is every `BlockingQueue` and every thread pool.

## Next Step 2 — "check whether the completion now takes a longer route"

No. The completion route is unchanged; it is the primitives underneath it. The
neighbouring `websocket-client-completion-on-app-thread-deadlock` fix is not
implicated.

## The defect that was found and fixed

`AbstractQueuedSynchronizer.acquire` spins up to 255 `Thread.onSpinWait()`
rounds before parking. `onSpinWait` measured **807 ns per call** against
HotSpot's 44 ns, so one handoff could burn ~200 us in the spin alone.

The reason was not the spin: **the invokestatic inline cache had been globally
suppressed since 2026-07-04.** `execute_invokestatic` set
`loader_specific_dispatch = true` on *attempting* loader-aware owner resolution
rather than on *selecting* one, and when `loader_aware_resolution()` became
default-on that predicate started returning `true` unconditionally — so every
invokestatic whose constant-pool owner was not the caller class was never
promoted, and re-ran a constant-pool resolve, three string-keyed native-registry
probes and a superclass walk under the class-manager `RwLock` on every call.

Fixed on branch `fix/tomcat-ws-async-seq2-20260803`, together with making
`Thread.onSpinWait()` a genuine interpreter intrinsic (its JDK body is empty and
HotSpot lowers it to one `PAUSE`):

| | before | after |
|---|---|---|
| `Thread.onSpinWait()` | 807 ns | **109 ns** |
| `Math.abs(int)` (already an intrinsic) | 814 ns | **513 ns** |
| interpreter intrinsic dispatches | 11.0M | **33.0M** |
| `Condition.signal` → `await` | 118.5 us | **96.9 us** |
| `ThreadPoolExecutor.execute` → task | 183.0 us | **167.1 us** |

16 Tomcat classes weighted toward the custom-classloader identity paths that
suppression protected show identical verdicts before and after, with 5-8% lower
wall times.

## Why the test still fails

The fix removes one contributor; the remaining gap is the general per-call
floor. An empty static call in a user class costs 166 ns interpreted here
against 0.2 ns inlined on HotSpot. Until that closes, a 255-round AQS spin
cannot cost less than tens of microseconds and a handoff cannot approach
HotSpot's single-digit microseconds. That is the subject of
[`aqs-thread-handoff-latency-20260803`](../../../known-issues/vm/aqs-thread-handoff-latency-20260803.md),
which carries the SEQ2 blocker forward.

## Corrections to the original doc

- Its Linux table (`SEQ2` 420-460, `nosel`/`sel` arms) and its conclusion that
  "the selector fix helps SEQ1 and is neutral on SEQ2" both hold. On Windows the
  same test gives SEQ2 483-499 — worse, same story.
- **Host load invalidates every number in this family.** An early pass of this
  investigation measured `execute` at 558 us and `Condition` at 228-494 us with
  a `cargo build` running; on a quiet host the same binary gives 183 us and
  118 us. HotSpot itself FAILS this test under build load (SEQ0 breaches 21 of
  an allowed 1). Always interleave, always repeat, always on a quiet box.
- The doc's "wall time is a second reading of the same thing" is right and
  useful: CratonVM 29.7-30.2 s vs HotSpot 26.8-27.0 s on Windows.

## Reproduction

```powershell
# the test, interleaved A/B, both arm orders
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance -Exe <exe>

# the primitives underneath it
javac -d out probes\HandoffLayersProbe.java probes\ExecDispatchProbe.java
java -cp out HandoffLayersProbe          # HotSpot control
<cratonvm.exe> --java-home <jdk> -cp out HandoffLayersProbe
```
