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

## Root cause (to pin down)
The record-batch iterator computes a file position/offset that goes negative on
CratonVM where HotSpot stays ≥0. Candidates:
- a batch `sizeInBytes`/`position` computed from an `int` that **overflowed** or was
  read with wrong endianness/width (`ByteBuffer.getInt/getLong`), then used as a seek
  target;
- `FileChannel.transferTo`/`position` arithmetic using a wrong base;
- a `FileLogInputStream` advancing `position += batchSize` where `batchSize` came back
  negative (a magic/size field mis-decoded), then the next `seek(position)` underflows.

Connects to the protocol-decode/`ByteBuffer` width bugs seen elsewhere (bug-18 Uuid,
bug-25 array sizes). Pin down by logging the computed seek target vs HotSpot for the
first failing `FileLogInputStream.nextBatch()`.

## Reproduce
```
cd apps/kafka/tests
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./kv2.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.record.FileLogInputStreamTest
```
