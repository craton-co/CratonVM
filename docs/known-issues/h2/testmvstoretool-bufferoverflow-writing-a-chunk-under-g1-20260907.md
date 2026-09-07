# `TestMVStoreTool` throws `BufferOverflowException` writing a chunk under G1, where HotSpot passes

| | |
|---|---|
| **Status** | **OPEN, unattributed.** Recorded because it is a Java-visible failure with a clean HotSpot oracle and nobody has filed it. Not root-caused. |
| **Scope** | `org.h2.test.store.TestMVStoreTool`, `-Xmx256m -XX:+UseG1GC`, Windows, corpus rebuilt 2026-09-06. One of several ways this class fails on CratonVM. |
| **Oracle** | HotSpot 25.0.3, same classpath, `rc=0`. |

```
Exception in thread "main" org/h2/mvstore/MVStoreException: java.nio.BufferOverflowException
  at org/h2/mvstore/MVStore.panic(MVStore.java:515)
Caused by: java/nio/BufferOverflowException
  at org/h2/mvstore/FileStore.serializeToBuffer(FileStore.java:1480)
  at org/h2/mvstore/Page$NonLeaf.writeUnsavedRecursive(Page.java:1392)
  ...
  at org/h2/mvstore/Page$Leaf.writeValues(Page.java:1698)
  at org/h2/mvstore/type/ObjectDataType.write(ObjectDataType.java)
  at org/h2/mvstore/DataUtils.writeStringData(DataUtils.java:316)
  at java/nio/HeapByteBuffer.put(HeapByteBuffer.java:220)
```

H2 sizes the chunk buffer from what it expects to write, then writes. An
overflow means the two disagree — so the suspects are whatever CratonVM
computes differently about a `String`'s length or a `ByteBuffer`'s remaining
capacity on this path, NOT the GC.

## What has been ruled out

* **Not this branch's holder screens.** Reproduces identically with
  `CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN=0` and zero refusals.
* **Not the rebuilt corpus.** HotSpot passes `rc=0` on the same classpath.

## What has NOT been ruled out

Whether it is a face of heap corruption or an independent `nio`/`String`
defect. A corrupted `String` — one whose `count` no longer agrees with its
backing array — would produce exactly this, and this class also produces the
G1 corrupt-header family on the same host. The distinguishing run is this class
under a collector that does not move objects, long enough to reach the write:
both non-G1 arms tried so far were TIMEOUTS at 1800 s and are not verdicts.
See `testmvstoretool-never-finishes-its-create-phase-on-cratonvm-20260907.md`
for why that budget is not enough.

**Do not quote this page as a GC defect until that run exists.** The stack is
in `nio`, and the only thing tying it to the collector is co-location on one
workload.
