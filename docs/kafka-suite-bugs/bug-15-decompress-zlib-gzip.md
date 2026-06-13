# Bug 15 — record GZIP decompress failure: `FilterOutputStream.close()` did not propagate to the wrapped stream

## ROOT-CAUSED + FIXED (2026-06-12)

**Not a zlib bug** — raw `Deflater`/`Inflater`/`GZIPOutputStream` round-trips are
correct on CratonVM. The defect is in `java.io.FilterOutputStream.close()`.

## Symptom
```
=> org.apache.kafka.common.KafkaException: Failed to decompress record stream
   Caused by: java.io.EOFException: Unexpected end of ZLIB input stream
```
`ProducerBatchTest.testSplitPreservesHeaders` / `testSplitPreservesMagicAndCompressionType`
(both go through `ProducerBatch.split()` → re-compress). The *written* batch was
truncated: a kafka `MemoryRecords` built with `CompressionType.GZIP` had
`sizeInBytes=71` on CratonVM vs `212` on HotSpot — only the 10-byte gzip header
plus the batch header, no deflated body or trailer. Reading it back hit EOF.

## Root cause (bisected to a one-liner)
`MemoryRecordsBuilder` writes records through
`DataOutputStream(compressionType.wrapForOutput(byteBufferStream, magic))` and
relies on `appendStream.close()` propagating down to `GZIPOutputStream.finish()`.

Minimal repro (`apps/kafka/tests/repro/W4.java`):
```java
DataOutputStream d = new DataOutputStream(new GZIPOutputStream(baos, 8192));
for (int i=0;i<2000;i++) d.writeByte(orig[i]);
d.close();                      // CratonVM: baos has 10 bytes; HotSpot: 308
```
`d.close()` → `DataOutputStream` has no `close()` → inherits
`FilterOutputStream.close()` (real bytecode: `flush(); out.close();`). But CratonVM
registered a **no-op `close` native on the base `java/io/OutputStream`**
(`native_baos_close`, for ByteArrayOutputStream), and that base native **shadowed**
the inherited `FilterOutputStream.close()` for any FilterOutputStream subclass
without its own `close` native (DataOutputStream, BufferedOutputStream). So
`DataOutputStream.close()` did nothing → the wrapped `GZIPOutputStream.finish()`
never ran → deflated body + trailer were never written. (`GZIPOutputStream.close()`
itself worked because the synthetic GZIPOutputStream has its own close/finish.)

Proof: calling `GZIPOutputStream.finish()` explicitly instead of relying on the
DataOutputStream close yields the full 321 bytes.

## Fix (`native-io/src/lib.rs`)
Register a real `java/io/FilterOutputStream.close()` native that mirrors the JDK:
flush this stream, then close the wrapped `out` (slot 0); idempotent via the
`closed` boolean at slot 1. This makes DataOutputStream / BufferedOutputStream /
any FilterOutputStream subclass propagate `close()`/`finish()` correctly, instead
of resolving to the base `OutputStream.close` no-op.

## Status
- Committed `8bb77a77`. Repros: `W4.java` (DOS→308), `MRGzip.java` (10 records
  round-trip), `BBOS.java`, `DOS.java`, `W1/W2/W3.java`.
- Affected: `producer.internals.ProducerBatchTest` + any compressed-record path
  (common.record / common.compress MemoryRecords GZIP). Append from the full run.
