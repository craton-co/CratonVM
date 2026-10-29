# Waking a parked thread takes ~3x HotSpot's time (TestAsyncMessagesPerformance, the timing-asserting classes)

| | |
|---|---|
| **Status** | **OPEN**, measured 2026-10-05 on `cratonvm-tomcatjitonly-g1-12..17.exe` (`--jdk-only`, Windows 11, JIT on and off, ZGC / Serial / G1 / Generational alike). Not caused by the DoHead / NonBlockingAPI fixes (docs 42-44). |
| **Affected** | `TestAsyncMessagesPerformance` (fails on every collector, CratonVM only; HotSpot `OK`). Timing-sensitive classes that pass standalone but fail under full-suite load: `TestELParserPerformance`, `TestManagerWebapp`, `TestOcspTimeout`; and the intermittent [TestWsWebSocketContainerTimeoutClient](../../internal/fixed-suite-bugs/tomcat/57-testwswebsocketcontainertimeoutclient-intermittent-close-race-bufferoverflow-not-seen-in-160-runs-on-the-windows-channel-layer-RETIRED-20261014.md). |
| **Probes** | `WakeCurve`, `WakeTimed`, `WakeWarm`, `WakeCold`, `AioPing`, `AioBurst`, `Handoff`, `SelLat` (not committed; each is a short `System.nanoTime` loop around one hand-off). |

## Symptom

`TestAsyncMessagesPerformance` asserts that over 500 iterations the gap between the two 8 KiB halves of a 16 KiB WebSocket message is under 0.5 ms (at most 10 misses) and the gap between the 16 KiB message and the next 4 KiB message is under 0.5 ms (at most 100 misses).
CratonVM misses 35-54 and 306-472 of 500 respectively (gaps of 0.5-5 ms); the 50 ms inter-iteration pause is also sometimes "short" (the client falls behind and catches up in a burst). Same result with the JIT off.

## What was measured

Round trip of two thread wake-ups after the waiter really slept (p50, us):

| | CratonVM | HotSpot |
|---|---|---|
| `Semaphore` ping-pong, hot (no sleep) | 14-27 | 28 |
| `Semaphore` ping-pong, 5-200 ms idle | 266-429 | 63-74 |
| raw `LockSupport.park/unpark`, 50 ms idle | 183-223 | 54-62 |
| same, pinned to one core | 111-115 | 24-30 |
| same, 30,000 hot iterations first (JIT-warm) | 157-166 | 54-69 |
| same, `-NoJit` | 117-138 | - |
| `parkNanos` (timed, Windows event path) vs `park` | 213 vs 174 | 71 vs 61 |
| thread pool `execute` -> run, 50 ms idle | 525 | 105 |

Ruled out, each measured:

* **Not the OS and not the condvar.** A standalone Rust program with the same `parking_lot` `Mutex`+`Condvar` (timed 5 ms waits) measures ~50 us for the same round trip; the unpark-to-park-return histogram inside the VM (`ParkState`) is mostly under 50 us; the timed (event) path is no faster than the untimed (condvar) path. Notifying after the lock release changed nothing.
* **Not the process.** Idle CPU 16 ms per 3 s (no spinning thread); no `SetPriorityClass`/`timeBeginPeriod`/affinity calls in the tree; not migration (pinning scales both VMs); not the JIT (JIT-warm and `-NoJit` both slow); not the collector.
* **Not the park prologue.** `CRATONVM_DBG_PARK_SKIP` over `tlab.retire`, the root-snapshot deposit, the `parkBlocker` pin and the `jdk.ThreadPark` event: no change in the wake latency (those run before the thread sleeps).
* **Not the Selector.** `Selector.wakeup()` -> `select` returns: 218 us vs 119 us; socket write -> read-ready: 562 vs 390 us.
* **Not the client AIO reads.** `AioBurst`: chunk-to-chunk gap p50 48-61 us on CratonVM, 61 us on HotSpot.

What is measurable: after a park returns, `park_with_state` runs `check_post_block_gc`, which ends in `deposit_root_snapshot_no_flag` (a full rebuild of the thread's root snapshot: frame scan, JIT band scan, class owners, SATB flush). Timed with `Instant` around the phases (temporary build 16, `CRATONVM_DBG_PARK_CP`): **26-28 us per cold park** (10 us averaged over the hot ones) for `check_post_block_gc` + root unpin; leaving the blocked region itself is under 1 us and the JFR tail 0.7 us. A wake-and-reply crosses two of them (~55 us of the ~190 us round trip); the OS wake (2 x 25-40 us) and the Java glue (`Unsafe.unpark`/`park` natives at ~250 ns each plus AQS) make up the rest, against HotSpot's 54-62 us total.

Skipping the refresh when `fixup` is empty and no fold targeted the thread would remove most of the 26 us but changes the GC root protocol (`root_publish_seq` consumers, `reclaim_guard::note_root_publish`); it was NOT attempted without a GC-stress validation round.

## Update 2026-10-05 (later): the bracket outside parks, and what did not move

The ~26 us post-block refresh above is the same `end_blocking_region` cost that every native which MAY park pays on the way out. [Doc 51](../../internal/fixed-suite-bugs/tomcat/51-blocking-region-bracket-around-calls-that-cannot-block-udp-receive-30x-hotspot-FIXED-20261005.md) removed it from the calls that cannot park (datagram receive/send of a queued datagram, non-blocking `SocketChannel` read/write: 14-45 us -> 0.7-3 us), and made a timed `Object.wait(1)` 13 ms -> 1.5 ms. It does not touch the park path itself, and `TestAsyncMessagesPerformance` is unchanged on `g1-22` (303-347 of 500 gaps over 0.5 ms; g1-17: 306-472), so the wake of a thread that really slept remains the open lever. A related 10-150 ms stall of the Tribes multicast receiver thread was described on [the multicast page](../../internal/fixed-suite-bugs/tomcat/62-testmulticastpackages-async-send-message-loss-FIXED-20261025.md) and is [fixed](../../internal/fixed-suite-bugs/tomcat/60-callee-door-compiled-the-callee-on-the-mutator-holding-its-locks-100-ms-process-wide-stalls-multicast-burst-lost-4-7-percent-FIXED-20261025.md) (a compile on the calling thread, not a wake latency).

## Why this is the 2-5 ms in the test

Tomcat's async send hands each frame through a `Semaphore`, the `SocketWrapper` completion (an executor hop) and the client's AIO reader. Each hop is one or two cold wake-ups (~190 us each here, 60 us on HotSpot) plus cold Java on a path that runs only 500 times (`LinkedBlockingQueue`/`ThreadPoolExecutor`/AQS in C1 or the interpreter, see [the native-call page](jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md)). Four to six hops explain 1-3 ms; the 5 ms tail is not accounted for.

## What would close it

`WakeCurve` idle=50 ms `Semaphore` p50 within ~1.5x of HotSpot (<= ~110 us) and `TestAsyncMessagesPerformance` `OK` on all three collectors. The two levers found: the post-wake snapshot refresh (26 us/park) and the cold-path speed of the Java glue.

## Update 2026-10-06: still failing on the `dev` tip

`TestAsyncMessagesPerformance` still fails (`AssertionError`, ZGC) on `cratonvm-tomcatjitonly-g1-27.exe`, the `dev` tip of 2026-10-06 (with its wait/notify hand-over work) merged with docs 42-52. Nothing on this page was closed by it.
