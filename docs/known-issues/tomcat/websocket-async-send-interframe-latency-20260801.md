# `TestAsyncMessagesPerformance`: the gap between two async WebSocket messages

**Status:** OPEN, root cause not found. **HotSpot:** PASS. Found while closing
[`wsremoteendpoint-server-close-never-completes-FIXED.md`](../../internal/fixed-suite-bugs/tomcat/wsremoteendpoint-server-close-never-completes-FIXED.md);
**it is not part of that chain and neither of that doc's two fixes moves it.**

## Symptom

`org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance.testAsyncTiming`
fails on CratonVM and passes on HotSpot. The test's own header says failures
need checking against the thresholds, so the thresholds are quoted in full
below — this one is over them by 4x, not marginally.

The server endpoint (`TesterAsyncTiming`) runs 500 iterations of

```java
semaphore.acquire(1);
remote.sendBinary(LARGE_DATA, handler);   // 16 KB, arrives as 2 x 8 KB chunks
semaphore.acquire(1);
remote.sendBinary(SMALL_DATA, handler);   // 4 KB
Thread.sleep(50);
```

and the client times the gaps between the chunks it receives:

| | what it measures | budget | breaches allowed (of 500) |
|---|---|---|---|
| SEQ0 | the 50 ms pause before the next 16 KB message | `> 40 ms` | 1 |
| SEQ1 | between the two 8 KB chunks of one 16 KB message | `< 0.5 ms` | 10 |
| SEQ2 | between the 16 KB message and the 4 KB message after it | `< 0.5 ms` | **100** |

## Measured

Interleaved, 4 rounds per arm, host load 6.6–15 throughout.
`nosel` = `origin/dev@492ddedd2`; `sel` = the same plus the selector `OP_WRITE`
fix from the doc above.

| arm | SEQ0 | SEQ1 | SEQ2 | wall time | verdict |
|---|---|---|---|---|---|
| HotSpot | 0, 0, 0, 0 | 2, 2, 1, 0 | 65, 32, 21, 0 | 25.6–26.4 s | PASS 4/4 |
| CratonVM `nosel` | 4, 4, 1, 0 | 68, 45, 25, 6 | **443, 460, 420, 426** | 28.6–32.3 s | FAIL 4/4 |
| CratonVM `sel` | 8, 4, 0, 0 | 59, 16, 7, 3 | **428, 435, 373, 409** | 28.6–31.4 s | FAIL 4/4 |

**SEQ2 is the failure**: ~80% of the 500 message boundaries exceed the 0.5 ms
budget, against a 20% allowance and HotSpot's 0–13%. Observed breach values run
0.51 ms to 2.69 ms. The selector fix helps SEQ1 and is neutral on SEQ2, so it is
a different mechanism.

Wall time is a second reading of the same thing. The test is 500 x 50 ms = 25 s
of sleeping, so the work is HotSpot ~1.4 s vs CratonVM ~5 s over 1500 chunk
sends — roughly 0.9 ms vs 3.3 ms per message.

## What it is not

**Not thread-handoff latency.** SEQ2 is bounded below by one
`Semaphore.release()` -> `acquire()` handoff, because the endpoint waits for the
16 KB message's completion callback before issuing the 4 KB one. Measured
directly with `probes/HandoffLatencyProbe.java`, 20,000 rounds, same host:

| | Semaphore release→acquire | LockSupport unpark→park | Object notify→wait |
|---|---|---|---|
| HotSpot | median 3.1 us, p99 15 us | median 8.6 us, p99 18 us | median 8.6 us, p99 17 us |
| CratonVM | median 8.5 us, p99 158 us | median 12.8 us, p99 22 us | median 13.0 us, p99 25 us |

CratonVM is 2.7x slower at the median and 10x at p99 — and still two orders of
magnitude under the 500 us budget. The handoff is real overhead but it cannot
produce a 0.5–2.7 ms gap, so the cost is inside the WebSocket send path itself:
either `sendBinary` issue → bytes on the wire, or wire-completion →
`SendHandler.onResult`.

**Not the selector `OP_WRITE` defect.** Fixed in the doc above; SEQ2 unchanged
(420–460 → 373–435, overlapping ranges).

**Not `TestWsRemoteEndpointImplServerDeadlock`.** That test passes 4/4 on the
same binary in the same window.

## Next steps

1. Split SEQ2 into its two halves — time-stamp inside
   `WsRemoteEndpointImplBase.startMessage`/`endMessage` and in the completion
   dispatch — to find out whether the 0.5–2.7 ms is spent issuing the next write
   or delivering the previous completion.
2. If it is the completion side, the neighbouring
   [`websocket-client-completion-on-app-thread-deadlock-FIXED.md`](../../internal/fixed-suite-bugs/tomcat/websocket-client-completion-on-app-thread-deadlock-FIXED.md)
   changed exactly that dispatch; check whether the completion now takes a
   longer route.
3. The p99 158 us on `Semaphore` (vs HotSpot's 15 us) is worth its own look
   regardless — it is a 10x on a primitive every AQS-based Java workload uses.

## Reproduction

```
cd /data/data/wsdead-probes
bash asy-ab.sh 4 /home/victor/jdk25/bin/java hs <exe> cvm      # interleaved A/B
/home/victor/jdk25/bin/javac -d hprobe HandoffLatencyProbe.java # handoff probe
```

`asy-ab.sh` takes `<rounds>` then `<exe> <tag>` pairs, runs only
`TestAsyncMessagesPerformance`, and reports the SEQ0/SEQ1/SEQ2 breach counts and
host load per run. It sets the grouped `CRATONVM_REAL=net-sockets,aqs
CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache` spelling — the
retired per-flag form is silently ignored by current binaries.
