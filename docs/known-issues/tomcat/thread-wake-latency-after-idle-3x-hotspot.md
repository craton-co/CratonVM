# Waking a parked thread takes ~3x HotSpot's time (TestAsyncMessagesPerformance, the timing-asserting classes)

| | |
|---|---|
| **Status** | **OPEN**, measured 2026-10-05 on `cratonvm-tomcatjitonly-g1-12..17.exe` (`--real-jdk`, Windows 11, JIT on and off, ZGC / Serial / G1 / Generational alike). Not caused by the DoHead / NonBlockingAPI fixes (docs 42-44). |
| **Affected** | `TestAsyncMessagesPerformance` (fails on every collector, CratonVM only; HotSpot `OK`). Timing-sensitive classes that pass standalone but fail under full-suite load: `TestELParserPerformance`, `TestManagerWebapp`, `TestOcspTimeout`; and the intermittent TestWsWebSocketContainerTimeoutClient. |
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

The ~26 us post-block refresh above is the same `end_blocking_region` cost that every native which MAY park pays on the way out. Doc 51 removed it from the calls that cannot park (datagram receive/send of a queued datagram, non-blocking `SocketChannel` read/write: 14-45 us -> 0.7-3 us), and made a timed `Object.wait(1)` 13 ms -> 1.5 ms. It does not touch the park path itself, and `TestAsyncMessagesPerformance` is unchanged on `g1-22` (303-347 of 500 gaps over 0.5 ms; g1-17: 306-472), so the wake of a thread that really slept remains the open lever. A related 10-150 ms stall of the Tribes multicast receiver thread was described on the multicast page and is fixed (a compile on the calling thread, not a wake latency).

## Why this is the 2-5 ms in the test

Tomcat's async send hands each frame through a `Semaphore`, the `SocketWrapper` completion (an executor hop) and the client's AIO reader. Each hop is one or two cold wake-ups (~190 us each here, 60 us on HotSpot) plus cold Java on a path that runs only 500 times (`LinkedBlockingQueue`/`ThreadPoolExecutor`/AQS in C1 or the interpreter; see `docs/internal/defects-and-fixes/jit-native-call-crossing-250ns-threadlocal-get-2us.md`, retired). Four to six hops explain 1-3 ms; the 5 ms tail is not accounted for.

## What would close it

`WakeCurve` idle=50 ms `Semaphore` p50 within ~1.5x of HotSpot (<= ~110 us) and `TestAsyncMessagesPerformance` `OK` on all three collectors. The two levers found: the post-wake snapshot refresh (26 us/park) and the cold-path speed of the Java glue.

## Update 2026-10-06: still failing on the `dev` tip

`TestAsyncMessagesPerformance` still fails (`AssertionError`, ZGC) on `cratonvm-tomcatjitonly-g1-27.exe`, the `dev` tip of 2026-10-06 (with its wait/notify hand-over work) merged with docs 42-52. Nothing on this page was closed by it.

## Update 2026-11-17: Linux wake/snapshot stalls resolved; Windows IOCP residual open

### What was fixed and verified

1. **Collector-uniform lazy JIT chain deferral & default-ON park/wake fast paths**:
   - `vm/src/vm/vm_exec/part_01.rs`: `park_deposit_defers_jit_chain` now enables deferred JIT-chain demotion across ZGC and Generational as well as G1.
   - `vm/src/threading/thread_registry/part_02.rs` and `types/src/flag_groups.rs`: flipped the 10 measured park/wake/sleep optimizations to default ON (`runtime_flag_default_on` / `off_word: Some("0")`): `park-lazy-stw-wake`, `park-deposit-frozen-band`, `blocking-leave-key-reuses-buffers`, `park-atomic-permit`, `unpark-eetop`, `park-wake-after-unlock`, `park-untimed-event`, `park-event-signal-after-unlock`, `park-event-lockless-wake`, `sleep-high-resolution-interval`.
2. **Eliminated `frozen_pages` multi-millisecond/480 us allocation stalls after idle**:
   - `vm/src/runtime/interpreter/gc_and_alloc/part_06.rs`: removed synchronous `frozen_pages::refresh_facts_if_due` from `safepoint_check` so waking/allocating threads do not stall on full heap walks right after `Thread.sleep` / `LockSupport.park`.
   - `jit/src/frozen_facts.rs` and `vm/src/runtime/frozen_pages.rs`: added `want_epoch` gating on `FrozenFactsState`/`FrozenVmState` so `dirty == false` backoff polls no longer loop indefinitely every ~25-31 ms scanning pins, and switched internal maps/sets in `frozen_pages` to `FxHashMap`/`FxHashSet`. On `AllocSplitAB` (first `new Node()` after 50 ms / 5 ms sleep), prior 4.2-9.6 ms spikes and the 480-980 us plateau dropped to 16-36 us (ZGC) and 9-21 us (G1).
3. **`InNativeRepeat` above-chain matching across repeated parks & `--compatible` boot fix**:
   - `vm/src/runtime/frame.rs`, `vm/src/runtime/value_stack.rs`, and `vm/src/native/jni/{part_01.rs,part_02.rs}`: enabled `blocking_repeat` and keyed `Frame::push_repeat_key_words` on method identity and frame state instead of ephemeral `frame.seq` (`CRATONVM_DBG=jni-repeat-audit` passed with zero mismatches).
   - `types/src/boot_image/key.rs`: `boot_image::cache_dir` reads `XDG_CACHE_HOME`/`HOME` via `std::env::var_os` directly so `--compatible` `reexec_without_aslr()` does not latch `VmFlags` before `install_flags()`.
4. **Windows `nio_file` classification & IOCP worker pool / test modules**:
   - `native-builtins/src/phases_late/nio_file/{part_04.rs,part_07.rs}`: restored explicit `NativeKind::Bridge` registrations (17 + 15 entries).
   - `native-io/src/net_iocp_win.rs`: added a bounded warm `WorkerPool` (`MIN_WARM_WORKERS = 8`) in `spawn`, fast non-blocking queue pop in `iocp_get` before `begin_blocking_region_as`, and an immediate `read0` path when `socket_available_stream` is positive; fixed `#[path = ...]` test module attributes in `net_iocp_win.rs`, `net_wepoll.rs`, and `net/nio_real/win.rs`.

Linux probe results (`x86_64` release build, back-to-back vs OpenJDK 25.0.5):
- `WakeCurveProbe 50 40` (`idle=50ms`, cold 40 iters, p50 / p90):
  - `LockSupport.park`: ZGC 43.3 / 68.2 us, G1 56.0 / 91.2 us, `--compatible` 41.2 / 54.6 us (HotSpot 17.9 / 33.6 us)
  - `parkNanos(5s)`: ZGC 34.8 / 59.5 us, G1 38.6 / 101.2 us, `--compatible` 38.0 / 78.9 us (HotSpot 18.5 / 24.6 us)
  - `Semaphore`: ZGC 136.9 / 219.5 us, G1 149.4 / 222.9 us, Gen 131.8 / 182.2 us, `--compatible` 271.9 / 338.5 us (HotSpot 30.9 / 69.3 us; down from 447 us / p90 13 ms)
  - `Executor`: ZGC 314.5 / 415.0 us, G1 292.6 / 397.1 us, `--compatible` 270.2 / 447.7 us (HotSpot 36.7 / 80.5 us)
- `WakeSplitProbe` (after 30k warmup, `idle=50ms`, p50 / p90):
  - `Semaphore`: ZGC 45.6 / 66.0 us, G1 31.9 / 61.9 us (HotSpot 22.9 / 37.0 us); `idle=0`: 0.9-1.0 us (HotSpot 6.2 us)
  - `Park`: ZGC 24.5 / 37.1 us, G1 36.6 / 54.2 us (HotSpot 23.4 / 35.0 us); `idle=0`: 0.5-0.7 us (HotSpot 6.1 us)
- `ExecWarmProbe` (after 30k warmup, `idle=50ms`, p50 / p90):
  - ZGC 55.1 / 140.3 us, G1 85.7 / 178.5 us (HotSpot 37.6 / 61.1 us); `idle=0`: ZGC 12.4 us, G1 3.1 us (HotSpot 6.5 us)

Unit tests verified: `cratonvm-types --lib` (715 passed, 1 ignored), `cratonvm-jit --lib -- frozen` (18/18), `cratonvm-vm --lib -- frozen_pages::tests r12_lock_block_state_tests i74_start_window_tests` (36/36), Windows `cratonvm-native-io --lib` (`net::iocp` 4/4, `net::wepoll` 3/3, `net::nio_real::win` 3/3).

### Remaining open items on Windows

- **Windows `PendingIoCache` overlapped mismatch on `connect0` / `accept0`**: On Windows `TestAsyncMessagesPerformance` fails before the timing loop with `DeploymentException` / `TimeoutException` at `WsWebSocketContainer.connectToServerRecursive` (`PendingFuture.get`), and standalone `AsynchronousServerSocketChannel`/`AsynchronousSocketChannel` probes (`AsyncConnectDiag`, `SplitAsyncDiag`, `IocpReflectDiag`) time out on both `accept0` and `connect0` across ZGC, G1, Generational, `--compatible`, and `--nojit`. Reflection inspection showed `Iocp` returns the `overlapped` value received by `connect0` (`0x1d176ae0000`) while `PendingIoCache.pendingIoMap` holds a different key (`0x1d174a90400`), leaving `EventHandlerTask` unable to match the `PendingFuture`.
- **Unverified Windows Tomcat runs & cold interpreter gap**: Because of the IOCP connect timeout above, Windows `TestAsyncMessagesPerformance` (500-iteration timing assertions) and `TestELParserPerformance`, `TestManagerWebapp`, `TestOcspTimeout`, and `TestWsWebSocketContainerTimeoutClient` remain unverified after these fixes. Cold 40-iteration `Semaphore`/`Executor` runs in the interpreter also remain slower than HotSpot (~132-149 us and ~270-315 us on Linux).
