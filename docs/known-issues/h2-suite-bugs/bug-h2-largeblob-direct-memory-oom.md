# H2 — `TestLargeBlob` hits `OutOfMemoryError: Direct buffer memory` under CratonVM where HotSpot doesn't

## Status
**OPEN** — one of the original 3 FAILs identified before the executor-shutdown
hang fix landed (`RESULTS-20260721.md`); now root-caused further.

## Severity
**MEDIUM** — affects any large-BLOB/LOB workload that pushes MVStore's
direct-buffer usage close to the JVM's `-XX:MaxDirectMemorySize` cap.

## Affected test class
`org.h2.test.db.TestLargeBlob` — PASSes on the HotSpot JDK25 baseline at the
same default suite heap (`--Xmx 1g`, which also bounds
`MaxDirectMemorySize` by default on both VMs since neither run passes an
explicit override).

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: IO Exception:
  "java.io.IOException: org.h2.mvstore.MVStoreException: java.lang.OutOfMemoryError:
   Direct buffer memory: tried 20193280, used 259891200, max 268435456 [2.4.249/3]"
	at org/h2/mvstore/db/LobStorageMap.createBlob(LobStorageMap.java:244)
	at org/h2/mvstore/FileStore.storeBuffer(FileStore.java:1550)
	at java/nio/DirectByteBuffer.<init>(DirectByteBuffer.java:108)
```
`used=259891200` (~248 MiB) against a `max=268435456` (256 MiB) direct-memory
cap — the store's async serialization/save executor (`FileStore`'s
background chunk-writer thread) tries to allocate one more ~19 MiB
`DirectByteBuffer` for a chunk write and the cap is nearly exhausted.

## Root cause (narrowed, not fully pinned down)
Not root-caused to a specific CratonVM defect in this session — this is
a genuine allocation hitting a real cap, not a corrupted/garbage size. Two
non-exclusive explanations remain open:
1. CratonVM's default `MaxDirectMemorySize` computation (when unset, real
   JDK defaults it to `-Xmx`) may resolve to a smaller effective value than
   HotSpot's for the same `--Xmx 1g` CratonVM invocation — worth comparing
   `-XX:+PrintFlagsFinal`-equivalent introspection (or
   `sun.misc.VM.maxDirectMemory()`) between the two.
2. Direct buffers freed by `DirectByteBuffer`'s `Cleaner`/`jdk.internal.ref.Cleaner`
   mechanism may not be reclaimed as promptly under CratonVM's GC as under
   HotSpot's (i.e. a slower- or non-triggering direct-memory reclaim path),
   letting "in-flight" direct memory usage climb higher before old buffers
   are freed, even though the workload's *peak legitimate* direct-memory
   need is the same on both VMs.

Distinguishing these needs either printing CratonVM's resolved
`MaxDirectMemorySize` directly, or instrumenting/counting live vs.
reclaimed `DirectByteBuffer` allocations across the run — not done this
session (time-boxed).

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 -Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestLargeBlob
```
