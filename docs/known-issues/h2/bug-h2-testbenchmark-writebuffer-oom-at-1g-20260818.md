# `TestBenchmark` — a 10 MB `WriteBuffer` grow throws `OutOfMemoryError` under `-Xmx1g`

| | |
|---|---|
| **Status** | OPEN, found 2026-08-18. |
| **Symptom** | `java.lang.OutOfMemoryError: Capacity: 10616832` raised inside `MVStore`'s background writer, which then calls `MVStore.panic`. |
| **Collectors** | ZGC FAIL at 28.9 s; Generational and G1 HANG at the 300 s cap (so the OOM is confirmed on ZGC; the other two never got far enough to say). |
| **HotSpot** | **PASSES**, same host, same `-Xmx1g`, in 169.5 s. |

## The number is the whole point

The failing allocation is **10,616,832 bytes — about 10 MiB — under a 1 GiB heap.** That is roughly 1% of the configured heap, requested by a buffer that is growing normally, and it fails:

```
Caused by: java/lang/OutOfMemoryError: Capacity: 10616832
    at org/h2/mvstore/WriteBuffer.grow(WriteBuffer.java:322)
    at org/h2/mvstore/Page$NonLeaf.writeUnsavedRecursive(Page.java)
    at org/h2/mvstore/FileStore.serializeToBuffer(FileStore.java:1480)
    at org/h2/mvstore/FileStore.serializeAndStore(FileStore.java:1447)
    at org/h2/mvstore/FileStore.lambda$storeIt$0(FileStore.java:1407)
    at java/util/concurrent/Executors$RunnableAdapter.call(Executors.java:545)
    at java/util/concurrent/FutureTask.run(FutureTask.java:328)
    at java/util/concurrent/ThreadPoolExecutor.runWorker(ThreadPoolExecutor.java:1090)
    at java/util/concurrent/ThreadPoolExecutor$Worker.run(ThreadPoolExecutor.java:614)
  -> org/h2/mvstore/MVStoreException.<init>(MVStoreException.java:18)
  -> org/h2/mvstore/DataUtils.newMVStoreException(DataUtils.java:996)
  -> org/h2/mvstore/MVStore.panic(MVStore.java:515)
```

**CratonVM fails FASTER than HotSpot passes** — 28.9 s against 169.5 s — because it gives up a fifth of the way in rather than doing the work slowly. This is not the wall-clock story that covers most of the H2 set (!nonpassed-40-census-20260818.md §2a); an arm that exits early is doing less work, not the same work slower.

`Capacity:` is the `java.nio.Buffer` allocation message, so the request is a `ByteBuffer` from `WriteBuffer.grow`, on an MVStore background-writer thread inside a `ThreadPoolExecutor`.

## What to establish first

1. **Is the heap actually full, or is the request being refused?** A 10 MiB buffer at `-Xmx1g` should be routine. If the heap genuinely is exhausted at that moment, the question is what filled it — and `TestBenchmark` is a benchmark, so a retention/liveness defect that HotSpot collects and CratonVM does not would show exactly here. If the heap is *not* full, this is an allocator refusal and the fragmentation/free-list path is the place to look — the ZGC arena's non-compacting free list has produced a "large free total, no single span big enough" refusal before, and that shape fits a 10 MiB contiguous request precisely.
2. **Which side of the direct/heap buffer split is it?** `Capacity:` narrows it to `java.nio`, and direct buffers are accounted separately from `-Xmx`.
3. **Whether the two HANG arms are the same defect.** Generational and G1 hit the 300 s cap without reaching this point, so they are currently unclassified; a longer cap would say whether they OOM the same way or are simply slower.

Note this is a *Java-level* OOM inside a live heap, and therefore unrelated to the heap **reservation** diagnostic fixed in `gc/src/arena.rs::alloc_zeroed_heap` (`a9c819011`) — that one covers a VM that cannot reserve `-Xmx` at startup. Raising a catchable `OutOfMemoryError` here is the correct behaviour; the defect is that it is raised at all.

## Reproduction

```bash
cd /data/h2gc-20260817
export CRATONVM_BIN=<cratonvm>  JDK25=/data/toolchain/jdk-25
export H2_ROOT=/data/cratonvm/apps/h2database/h2
export OUTROOT=$PWD/out-probe  H2_GC_FLAG='-XX:+UseZGC'
bash run-h2-suite.sh run --category all \
    --only '^org\.h2\.test\.store\.TestBenchmark\$' --max-heap 1g --class-to 600 --tag probe

# control — passes in ~170 s
bash run-h2-suite.sh hotspot --category all \
    --only '^org\.h2\.test\.store\.TestBenchmark\$' --max-heap 1g --class-to 600
```

Raise `--max-heap` as an A/B: if 2g clears it, that sizes the gap; if it does not, the allocator is refusing rather than the heap being full.
