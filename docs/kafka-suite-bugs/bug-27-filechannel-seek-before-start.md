# Bug 27 — File record reads: `IOException: seek before start of file`

**Severity:** Medium-High — record/file-log reading. HotSpot OK.

## Symptom
```
=> java.io.IOException: seek<N>: Попытка поместить указатель на файл перед началом файла. (os error N)
   ( = "attempt to move the file pointer before the beginning of the file" )
```
A `FileChannel.position(p)` / `RandomAccessFile.seek(p)` is called with a **negative or
underflowed** `p` while reading Kafka log segments, so the OS rejects it.

## Affected classes
`FileLogInputStreamTest` (30/77), `RemoteLogInputStreamTest` (23/60),
`UnalignedFileRecordsTest` (0/1). (`FileRecordsTest` TIMEOUTs — likely related.)

## Root cause — PINNED to `FileChannel.truncate`

Exact failing path (from the stack):
```
FileRecords.truncateTo(FileRecords.java:270)
  → sun.nio.ch.FileChannelImpl.truncate(FileChannelImpl.java:578)
    → sun.nio.ch.FileDispatcherImpl.seek(FileDispatcherImpl.java:88)
      → seek0: …before start of file (os error 131 = Windows ERROR_NEGATIVE_SEEK)
```
The test (`testBatchIterationIncompleteBatch`) truncates the segment file to forge an
incomplete batch. The **real JDK `FileChannelImpl.truncate` bytecode runs** — CratonVM
only overrides `truncate` on the *abstract* `java/nio/channels/FileChannel`
(`phases_late.rs:9342`), **not** on the concrete `sun/nio/ch/FileChannelImpl`, so the
override never fires. The real `truncate` then issues a seek to a **negative** absolute
offset (its position/size bookkeeping ends up < 0 on CratonVM) and the low-level
`seek0` rejects it (no native override for `sun/nio/ch/FileDispatcherImpl`
`seek0`/`size0`/`position0`).

### Fix direction
Register a native for **`sun/nio/ch/FileChannelImpl.truncate(J)Ljava/nio/channels/FileChannel;`**
(the concrete class) implementing the JDK contract: clamp `newSize ≥ 0`, truncate the
underlying file, and set the position to `min(currentPosition, newSize)` — never seek
negative. (Mirror the existing abstract-`FileChannel.truncate` native at
`phases_late.rs:9342` but key it on `FileChannelImpl`.) Alternatively, fix the
`size`/`position` primitives the real `truncate` reads so it never computes a negative
seek. Affects all three classes once `truncate` is correct.

## Reproduce
```
cd apps/kafka/tests
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./kv2.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.record.FileLogInputStreamTest
```
