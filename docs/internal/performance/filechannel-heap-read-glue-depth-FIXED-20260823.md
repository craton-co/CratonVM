# `FileChannel.read` into a heap buffer — FIXED 2026-08-23

| | |
|---|---|
| **Status** | **FIXED.** The twenty-frame JDK glue chain is one native call; the page's own probes moved 3.4-3.6x and the residual is a different, named page |
| **Opened** | 2026-08-22, re-homed out of the GPULlama3 record |
| **Closed by** | `perf/filechannel-vector-webclient-residuals-20260823` |
| **Measured effect** | `FileChannelHeapReadProbe` **39.9 -> 10.4 µs** per read pair (**3.8x**); `GgufStringReadProbe` **23.4 -> 7.0 µs** per string (**3.4x**). Gap to HotSpot: **19.6x -> 5.1x** |

## What the page said, and what was done about it

The open page priced `FileChannel.read` into a HEAP buffer at ~8.7x HotSpot and
root-caused it correctly: not the temporary-direct-buffer cache (measured and
refuted there, twice), but **call depth**. One `read(ByteBuffer)` walks about
twenty JDK frames, nothing in a 3519-sample profile is above 11%, and C2 folds
the whole chain into a handful of instructions around one syscall while
CratonVM runs it.

It then named the only change that would move the number — a native for
`FileChannelImpl.read(ByteBuffer)` — and **declined to take it**, for a reason
worth quoting because it is what the fix had to answer:

> A native fast path has to reproduce that exactly — the position advance, the
> `IOStatus.INTERRUPTED` retry, `IOStatus.normalize`, the EOF-is-`-1`
> convention, the read-only and non-readable refusals, and the
> `beginBlocking`/`endBlocking` pairing that makes the channel closeable by
> another thread mid-read. Getting any one of them wrong silently
> desynchronises the channel position, and every subsequent read on that
> channel returns the wrong bytes rather than failing.

`native-io/src/file_channel_fast_read.rs` answers it by **refusing**, and by
refusing to the method's own bytecode. `invoke_special_bytecode_only` is the
"run this body, no native check" primitive, so naming the very method the
native is registered for is not a recursion — it is the un-intercepted VM,
including `read`'s own `if (jfrTracing && FileReadEvent.enabled()) return
traceImplRead(dst);` branch, which a refusal to `implRead` would have silently
skipped.

Five methods are intercepted: `read(ByteBuffer)`, `write(ByteBuffer)`,
`position()`, `position(long)` and `size()`. The last three were not in the
page's plan and were added after the first measurement, because with the two
transfers collapsed the probe still read 19.6 µs — its loop calls
`ch.position()` once per iteration and that walks its own
`ensureOpen -> synchronized(positionLock) -> beginBlocking -> threads.add ->
nd.seek -> IOStatus.normalize -> threads.remove -> endBlocking` chain.

### What refuses

`jfrTracing` set; a closed channel; a non-`readable` (resp. non-`writable`)
channel; a `direct` (O_DIRECT) channel; a read-only, `MemorySegment`-backed or
direct destination buffer; a pending `interruptedTarget`; a calling thread
whose interrupt flag is already set; and a **virtual** calling thread, because
`VirtualThread.blockedOn` wraps its superclass body in
`disableSuspendAndPreempt`/`enableSuspendAndPreempt` and a fast path that
published the blocker without them would leave the thread preemptable between
the publication and the I/O.

### What is reproduced rather than skipped

* `positionLock` is held across the whole operation.
* `blockedOn(interruptor)` is published on the calling thread before the I/O
  and cleared after, so another thread's `Thread.interrupt()` still reaches
  `AbstractInterruptibleChannel$1.interrupt` and closes the channel. Two
  monitor operations and one field write; skipping it would have deferred an
  asynchronous close by one operation, which is a real weakening.
* `end(completed)`'s exceptional tail — `ClosedByInterruptException` when this
  thread was the interrupted target, `AsynchronousCloseException` when the
  channel closed under an incomplete operation — is delegated to the JDK's own
  `end`, but **only on the branch that can throw**, so the hot path enters no
  Java frame for it.
* EOF is `-1`, an empty destination is `0`, and the buffer's `position`
  advances by exactly the byte count.

One thing the page feared turned out not to exist. It said "on Windows a
`FileChannelImpl` does not hold an OS file position: it keeps its own, and
`FileDispatcherImpl.read` performs a `seek` to it and then a `ReadFile`". The
JDK 25 source says otherwise — `FileDispatcherImpl.read` is exactly
`return read0(fd, address, len)`, with no seek — and the `seek` frames in the
page's own profile are the PROBE's `ch.position()` call, not the read path.
The position advance is the OS handle's, on both platforms.

## The numbers

Azure Linux host 2, one binary, `CRATONVM_FC_FAST_IO=0|1` interleaved, three
rounds, Temurin 25.0.3+9 as the oracle on the same file:

| | round 1 | round 2 | round 3 |
|---|---:|---:|---:|
| `FileChannelHeapReadProbe`, ns per read pair | | | |
| fast I/O **off** | 41 867 | 35 784 | 42 109 |
| fast I/O **on** | **11 006** | **11 346** | **9 961** |
| HotSpot | 2 202 | 1 961 | 1 937 |

`GgufStringReadProbe`, the shape GGUF's `readString` uses (an 8-byte length
read then the bytes), same binary: **23.44 -> 6.97 µs per string**, HotSpot
1.52.

`checksum` is `-540000` in every CratonVM arm and on HotSpot;
`GgufStringReadProbe`'s is `-53403` in all three.

The engagement census is what makes those readable —
`CRATONVM_FC_FAST_IO_STATS=1`, from the "on" arm:

```
[cratonvm] filechannel fast I/O: read fast=120000 refused=0  write fast=64 refused=0
                                 pos fast=60000 refused=0  size fast=0 refused=0
```

120 000 reads is exactly 20 000 iterations x 2 reads x 3 passes, and
`refused=0` on every row that fired. Without this line "the native ran on every
read" and "every read refused while the host happened to be quieter" produce
the same wall clock.

## What is left, and why it is not this page

The gap to HotSpot is **5.1x**, down from 19.6x. What remains is not
`FileChannel`-shaped: per read pair the fast path makes three native calls
(~200 ns each at this VM's measured per-call cost) against a probe loop that
also allocates a `byte[]`, constructs a `HeapByteBuffer` through
`ByteBuffer.wrap`, and runs `clear()`/`get(0)`/array reads — all interpreted.
That is the general per-call and per-bytecode price, which
[[jit-entries-per-call-cost-is-the-call-dense-wall]] owns and which
`known-issues/perf/jit-compiled-caller-to-interpreted-callee-costs-1900ns-20260822.md`
is the live half of.

**Do not reopen this page for that.** The distinguishing test is the census: if
`read fast=N refused=0`, the twenty frames are gone and what is being measured
is the interpreter, not the channel.

## Repro

```bash
CRATONVM_FC_FAST_IO=0            # the un-intercepted VM (gates REGISTRATION)
CRATONVM_FC_FAST_IO_STATS=1      # the engagement census at exit
probes/FileChannelHeapReadProbe.java   # 20000 24
probes/GgufStringReadProbe.java
probes/DirectBufferCacheProbe.java     # 5000
```

`DirectBufferCacheProbe` now reports `direct_count_before=0` where the open
page recorded `1`. That is not the instrument going inert: the fast path never
borrows a temporary direct buffer at all, so the pool bean is never touched.
`per_read=0.0` with `CACHE_WORKING` is the same verdict for a different reason,
and the `read fast=N` census row is what distinguishes them.

Semantics are pinned by `RChannelInterrupt` and `RFileChannelIsOpen` in the
regression suite, which the page itself named as the acceptance evidence.
