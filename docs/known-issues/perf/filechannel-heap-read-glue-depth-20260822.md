# `FileChannel.read` into a heap buffer is ~8.7x HotSpot, and it is call depth

## Status

**OPEN**, characterised, not fixed. Re-homed on 2026-08-22 out of the
GPULlama3 record (resolved as
`gpullama3-model-load-and-ffm-segment-class-identity-RESOLVED-20260822.md`),
which carried this as a minor open gap and attributed it to a cause that is
now refuted.

## Severity

**LOW-MEDIUM.** No correctness consequence. Worth roughly 11 s of GPULlama3's
~70 s model load, and proportionally on any workload whose I/O is many small
`FileChannel.read(HeapByteBuffer)` calls — which is the shape every
`DataInput`-style binary format reader has.

## The measurement

`probes/FileChannelHeapReadProbe.java`, 20,000 reads of 24 bytes, best of
three passes. Temurin 25.0.3+9 as the oracle, same file, same host:

| VM | ns per read pair |
|---|---|
| HotSpot | 3 647 |
| CratonVM, JIT on | 31 769 / 33 285 (two runs) |
| CratonVM, `--nojit` | 55 085 |

**~8.7x** with the JIT on. `probes/GgufStringReadProbe.java` prices the same
shape as GGUF's `readString` does it (an 8-byte length read, then the bytes)
and moves with it.

The host was not idle for these — another build was running on it — so the
absolute numbers are upper bounds. The RATIO is the claim, and it held across
two CratonVM runs and reproduces the order the original record recorded.

## What it is NOT

The record this came from attributed the gap to `sun.nio.ch.Util`'s
per-thread temporary-direct-buffer cache not working: a `FileChannel.read`
into a HEAP buffer cannot DMA into the Java array, so the JDK borrows a
direct buffer, reads into it, and copies out. If that cache misses, every
read allocates a fresh `DirectByteBuffer` and registers a `Cleaner`. The
evidence offered was a stack sample in which "`DirectByteBuffer.<init>` and
`CleanerImpl.run` between them took 12 of 16 samples".

Two independent measurements refute it.

**The cache works.** `probes/DirectBufferCacheProbe.java` counts direct
buffers allocated per read through `BufferPoolMXBean("direct")`:

```
DBUFCACHE reads=5000 direct_count_before=1 direct_count_after=1
          direct_delta=0 per_read=0.0 used_delta_bytes=0
DBUFCACHE verdict=CACHE_WORKING
```

`direct_count_before=1`, not 0, is the part that makes this readable: the
counter is live and already holds a real buffer, so `delta=0` is a measured
zero and not an inert instrument. (Until 2026-08-22 it *was* an inert
instrument — the pool bean's four accessors were stateless lambdas returning
0, so this probe would have printed `CACHE_WORKING` no matter what the VM
did. That is fixed in the same change.)

**And the frames are not there.** A `--nojit --stack-sample-ms 3` profile of
`FileChannelHeapReadProbe 40000 24` — **3519 samples** — contains no
`DirectByteBuffer.<init>`, no `CleanerImpl.run`, and no
`Util.getTemporaryDirectBuffer` in the read loop at all.

## What it is

The 3519 samples, deepest frame per sample, top of the distribution:

```
382  sun/nio/ch/IOUtil.read
338  sun/nio/ch/FileChannelImpl.implRead
334  sun/nio/ch/FileDispatcherImpl.read
303  java/nio/channels/spi/AbstractInterruptibleChannel.blockedOn
287  FileChannelHeapReadProbe.main
248  java/nio/HeapByteBuffer.put
211  sun/nio/ch/IOUtil.readIntoNativeBuffer
191  java/nio/MappedByteBuffer.position
175  sun/nio/ch/FileChannelImpl.position
149  java/nio/MappedByteBuffer.flip
114  java/nio/channels/spi/AbstractInterruptibleChannel.begin
 92  sun/nio/ch/FileChannelImpl.ensureOpen
 80  java/nio/channels/spi/AbstractInterruptibleChannel.end
 73  sun/nio/ch/FileChannelImpl.beginBlocking
 71  sun/nio/ch/FileChannelImpl.endBlocking
 66  sun/nio/ch/IOUtil.bufferAddress
 66  sun/nio/ch/FileDispatcherImpl.seek
 57  sun/nio/ch/IOUtil.acquireScope
 53  sun/nio/ch/IOUtil.releaseScope
 51  sun/nio/ch/FileChannelImpl.read
```

Nothing is above 11%, and the list is simply the call chain a real
`sun.nio.ch.FileChannelImpl.read(ByteBuffer)` walks, in order:

```
read -> beginBlocking -> ensureOpen -> implRead -> IOUtil.read
     -> acquireScope -> readIntoNativeBuffer -> bufferAddress
     -> FileDispatcherImpl.seek/read -> HeapByteBuffer.put
     -> releaseScope -> endBlocking -> blockedOn
```

**This is a distribution, not a defect.** There is no lever worth 30% of it.
HotSpot pays the same twenty frames and they cost nothing there because C2
inlines the chain into a handful of instructions around one syscall; CratonVM
runs them. The JIT already buys 1.7x (55 µs `--nojit` -> 32 µs), which is the
size of the effect a better compiler makes here, and the remaining 8.7x is
what is left.

## The fix that was considered and not taken

Register a native for `sun/nio/ch/FileChannelImpl.read(Ljava/nio/ByteBuffer;)I`
that, for a heap destination, reads from the file descriptor straight into
the backing array — collapsing the twenty frames above into one native call.
That is the only change that would move this number materially.

It was not taken here, and the reason is worth stating so the next reader
does not have to re-derive it. On Windows a `FileChannelImpl` does **not**
hold an OS file position: it keeps its own, and `FileDispatcherImpl.read`
performs a `seek` to it and then a `ReadFile`, all under `positionLock`.
A native fast path has to reproduce that exactly — the position advance, the
`IOStatus.INTERRUPTED` retry, `IOStatus.normalize`, the EOF-is-`-1`
convention, the read-only and non-readable refusals, and the
`beginBlocking`/`endBlocking` pairing that makes the channel closeable by
another thread mid-read. Getting any one of them wrong silently desynchronises
the channel position, and every subsequent read on that channel returns the
wrong bytes rather than failing. That is a poor trade for 11 s of one app's
model load, on a surface every file read in the VM goes through.

If it is taken later, the acceptance evidence is already written:
`FileChannelHeapReadProbe`, `GgufStringReadProbe` and `DirectBufferCacheProbe`
against the HotSpot numbers above, plus `RChannelInterrupt` and
`RFileChannelIsOpen` in the regression suite for the semantics the fast path
would have to preserve.

## Repro

```bash
probes/FileChannelHeapReadProbe.java   # 20000 24
probes/GgufStringReadProbe.java
probes/DirectBufferCacheProbe.java     # 5000
```

Run each on CratonVM and on a real JDK 25 and compare `ns_per_read_pair`,
`us_per_string` and `per_read`. To re-take the profile:

```bash
cratonvm.exe --java-home <jdk25> --nojit --stack-sample-ms 3 \
  -cp <probe-out> FileChannelHeapReadProbe 40000 24
```

and aggregate the deepest frame of each `T19.H1 stack dump`. `--nojit` is not
optional: JIT-compiled frames never reach the dispatch loop and are invisible
to the sampler. The 16-sample JIT-on profile that produced the refuted
diagnosis above is what that looks like when it is trusted.
