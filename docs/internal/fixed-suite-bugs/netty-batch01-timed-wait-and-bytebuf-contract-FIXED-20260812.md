# netty batch-01: three JDK-contract divergences behind 30 buffer-test failures

**Status:** ✅ **FIXED** 2026-08-12 on `fix/netty-batch01-20260812`.
Found while triaging [`docs/known-issues/netty/investigate-batch-01.md`][b1]
(15 classes recorded FAIL/HANG on Windows at `70c8b8cd6`); reproduced,
root-caused and fixed on the Azure Linux host (`20.80.105.49`,
`/data/cvm-nb01-20260812`), branched from `origin/dev` `1c4ce7d3a`.

[b1]: ../../known-issues/netty/investigate-batch-01.md

## Result

Batch-01's 15 classes, same binary, HotSpot JDK 25 as oracle:

| | before | after |
| --- | --- | --- |
| classes matching HotSpot's per-test result exactly | 1 | **12** |
| distinct failing test methods | 34 | **2** |
| `io.netty.buffer` classes with an assertion failure | 10 | **0** |

The 2 remaining failures are `mustCallInitializerExtensions()` in
`BootstrapTest` / `ServerBootstrapTest`, which fail **identically on stock
HotSpot JDK 25** — a ServiceLoader/classpath artefact of this runner, not a
CratonVM defect. After the fixes there is **no test in this batch that fails on
CratonVM and passes on HotSpot.**

The other three classes differ only in which tests are *skipped* (a failed
`Assumptions.assumeTrue`), never in a failure; both causes are filed, see
*What is left*.

Collector-independent, as the original Windows run's identical default/G1/ZGC
statuses suggested: re-run under `-XX:+UseG1GC`,
`BigEndianHeapByteBufTest` 414/414, `BigEndianDirectByteBufTest` 413/413,
`AdaptiveBigEndianHeapByteBufTest` 415/417 (2 skipped),
`AdvancedLeakAwareByteBufTest` 426/426 — byte-identical to the default
collector and to HotSpot.

## 1. `ByteArrayInputStream.read(byte[],int,int)` checked `len == 0` before EOF

`native-io/src/lib.rs`, `native_bais_read_bytes`.

The JDK orders the two checks EOF-first:

```java
public synchronized int read(byte[] b, int off, int len) {
    Objects.checkFromIndexSize(off, len, b.length);
    if (pos >= count) return -1;          // <-- first
    int avail = count - pos;
    if (len > avail) len = avail;
    if (len <= 0) return 0;               // <-- second
    ...
}
```

CratonVM had them the other way round, so a zero-length read on an
already-drained stream answered `0` where HotSpot answers `-1`.

`AbstractByteBufTest.testStreamTransfer1` asserts exactly that
(`assertEquals(-1, buffer.setBytes(i, in, 0))` after the stream is drained) —
it failed on **all 10** concrete `ByteBuf` test classes in this batch.

The ordering is BAIS-specific: `InputStream.read(byte[],int,int)`'s default
implementation genuinely returns `0` for `len == 0` before it ever calls
`read()`. That is a different branch of the same native and was already
correct; the fix does not touch it.

A pre-existing unit test,
`offset_constructor_negative_length_is_immediate_eof_for_bulk_reads`, asserted
the wrong answer (`Int(0)`) — it had frozen the divergence. Corrected against a
stock HotSpot witness:

```
new ByteArrayInputStream(new byte[4], 0, -1)   // count = min(0 + -1, 4) = -1
read(dst, 0, 8) == -1    read(dst, 0, 0) == -1    available() == -1
```

## 2. A zero-length bulk copy at a direct buffer's limit threw

`native-builtins/src/unsafe_natives_ext.rs`, `ArenaStore::copy_in` /
`copy_out`.

Both resolve the address with `locate()`, an **exclusive** range test, so
`base + size` — one past the last byte — reports "not inside any live block"
and the caller turns that into an exception. But a **zero-length** copy touches
nothing and is legal at exactly that address, which is where a direct
`ByteBuffer` positioned at its own limit points.

Every `ByteBuffer` bulk accessor on a direct buffer inherits it. Witnessed
against HotSpot with a 20-line probe — CratonVM before the fix:

```
D1 java.lang.IllegalStateException: ByteBuffer.put: direct destination write failed
D2 java.lang.IllegalStateException: ByteBuffer.put([B): direct destination write failed
D3 java.lang.IllegalStateException: ByteBuffer.put([BII): direct destination write failed
D5 java.lang.IllegalStateException: ByteBuffer.put: direct source read failed
D9 java.lang.IllegalStateException: ByteBuffer.get([B): direct source read failed
```

all of which HotSpot completes as no-ops. It reached netty through
`AbstractByteBufTest.writerIndexBoundaryCheck4`, whose whole body is
`writeBytes(ByteBuffer.wrap(EMPTY_BYTES))` against a full direct buffer.

Fixed at the arena layer rather than at the six `ByteBuffer` call sites: the
rule "a 0-byte copy is in bounds anywhere" is a property of the copy, not of
`ByteBuffer`, and `Unsafe.copyMemory(..., 0)` at the end of a block has the
same claim on it. A non-empty copy past the end is still refused.

## 3. Every timed `java.util.concurrent` native read the TimeUnit's ordinal from slot 0

`native-builtins/src/{lib,util_concurrent_ext,phases_early}.rs` — **9 call
sites**.

`java.util.concurrent.TimeUnit` is a JDK enum: object slot 0 holds the `Enum`
`name` (a `String`) and the ordinal lives in the named `ordinal` field. Nine
natives did

```rust
Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
```

so `as_int()` failed on the `name` reference and the code silently took the
`unwrap_or(2)` fallback — **MILLISECONDS**. `await(30, SECONDS)` waited 30
*milliseconds*.

The correct reader, `time_unit_ordinal()`, already existed **and its doc
comment already described this exact bug** — it had only ever been wired into
one of the ten sites (`CountDownLatch.await`). The other nine:

| file | native |
| --- | --- |
| `util_concurrent_ext.rs` | `LinkedBlockingQueue.poll(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `ArrayBlockingQueue.poll(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `LinkedTransferQueue.poll(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `ReentrantLock.tryLock(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `Condition.await(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `Semaphore.tryAcquire(long, TimeUnit)` |
| `util_concurrent_ext.rs` | `Future.get(long, TimeUnit)` |
| `lib.rs` | `CyclicBarrier.await(long, TimeUnit)` |
| `phases_early.rs` | `Exchanger.exchange(V, long, TimeUnit)` |

This is what produced the batch's `CyclicBarrier await timed out` /
`BrokenBarrierException` cluster — 22 of the 34 failing test methods. It was
never a throughput problem: the work those tests do finishes in **623 ms** on
CratonVM (47 ms on HotSpot) against a barrier that was supposed to allow 30
seconds and actually allowed 30 milliseconds.

Reach is much wider than netty: any `poll`/`tryLock`/`tryAcquire`/`get`/
`exchange` with a SECONDS-or-coarser timeout was returning "timed out"
1000-86 400× early.

`probes`-style witness, CratonVM after the fix, identical to HotSpot line for
line:

```
cb_await30s_partnerAt1500ms trip1=OK after 1500ms
cb_awaitUntimed_partnerAt1500ms=OK after 1560ms
cb10_await10s_prompt=OK after 238ms
condition_awaitNanos30s_signalAt1500ms=SIGNALLED after 1501ms
condition_awaitNanos500ms_noSignal=elapsed=500ms
parkNanos500ms=elapsed=500ms
latch_await30s_countAt1500ms=true after 1500ms
```

## 3b. `CyclicBarrier` reported barrier failures as `IllegalStateException`

Same native. A timeout raised
`IllegalStateException("TimeoutException: CyclicBarrier await timed out")` and
a broken generation raised `IllegalStateException("BrokenBarrierException")` —
the right *words*, the wrong *type*. `CyclicBarrier.await` declares both as
checked exceptions and callers discriminate on the type (`catch
(TimeoutException)` to retry vs `catch (BrokenBarrierException)` to abandon),
so a correct caller propagated a fatal error where HotSpot recovers.

Now constructs the real `java.util.concurrent.TimeoutException` /
`BrokenBarrierException`, falling back to the historic
`IllegalStateException` only if the class cannot be built (synthetic-JDK mode),
so no configuration loses the failure entirely.

## Tests

* `native-io`: `bais_read_zero_len_at_eof_is_minus_one`,
  `bais_read_zero_len_empty_source_is_minus_one`,
  `bais_read_zero_len_before_eof_is_zero`; existing
  `offset_constructor_negative_length_is_immediate_eof_for_bulk_reads`
  corrected. `cargo test -p cratonvm-native-io --lib` 454 passed / 0 failed.
* `native-builtins`:
  `arena_zero_length_copy_tests::zero_length_copy_at_end_of_block_succeeds`
  (0-byte in/out at one-past-end succeed, interior copy round-trips, 1-byte
  past the end still refused). `cargo test -p cratonvm-native-builtins --lib`
  3495 passed / 3 failed — the 3 are `logmanager::tests::t19_h3_*`, which fail
  identically on a pristine `origin/dev` worktree (order-dependent global
  logger registry), unrelated to this change.

## What is left in batch-01

Not defects introduced or missed here — filed separately:

* `io.netty.bootstrap.{Bootstrap,ServerBootstrap}Test` —
  `mustCallInitializerExtensions()` fails **identically on stock HotSpot JDK
  25** (`AssertionFailedError: expected: <[id: 0x…]> but was: <null>`), a
  harness/ServiceLoader artifact of this runner's classpath, not a CratonVM
  defect. Nothing to fix.
* Wall-clock timeouts with no assertion failure in the three
  `AdaptiveByteBufAllocator*` classes and
  `AdaptiveBigEndianDirectByteBufTest.testInternalNioBuffer`. All of them
  **pass when run solo**; the gap is 10× broadly and ~90× through
  `AdaptivePoolingAllocator` →
  `known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`.
* `AlignedPooledByteBufAllocatorTest` runs 28 tests HotSpot skips, because
  CratonVM pins `sun.misc.unsafe.memory.access=allow` and netty therefore keeps
  `hasUnsafe() == true` where HotSpot 25 turns it off →
  `known-issues/netty/unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md`,
  and the reason that pin cannot just be dropped,
  `known-issues/netty/memorysegment-asbytebuffer-unimplemented-20260812.md`.
* `CyclicBarrier`'s barrier-action `Runnable` is silently dropped →
  `known-issues/netty/cyclicbarrier-native-drops-barrier-action-20260812.md`.
* `ManagementFactory.getThreadMXBean()` is not a `com.sun.management.ThreadMXBean` →
  `known-issues/netty/threadmxbean-not-com-sun-extension-20260812.md`.

## Repro (pre-fix)

```bash
cd apps/netty-suite-runner
printf 'io.netty.buffer.BigEndianHeapByteBufTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 \
  --bin <cratonvm> --out /tmp/repro
```
