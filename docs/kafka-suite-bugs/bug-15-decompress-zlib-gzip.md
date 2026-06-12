# Bug 15 — record decompression failure (`EOFException: Unexpected end of ZLIB input stream` / `Failed to decompress record stream`)

**Severity:** Medium — breaks compressed-record round-trips. Reproduces under
`--nojit`. HotSpot clean.

## Symptom
```
=> org.apache.kafka.common.KafkaException: Failed to decompress record stream
=> org.apache.kafka.common.KafkaException: java.io.EOFException: Unexpected end of ZLIB input stream
```
Producing a record batch with GZIP compression and reading it back fails to
inflate — the inflater hits EOF before the expected length.

## Root cause (to pin down)
CratonVM's `java.util.zip.Inflater`/`GZIPInputStream` (or `Deflater`/
`GZIPOutputStream`) round-trip is truncated/corrupted — the compressed stream
written by the producer path can't be fully inflated by the consumer path.
Candidates:
- `Deflater`/`Inflater` native (zlib) length/flush handling (finish() not flushing
  the trailer, or `deflate`/`inflate` byte accounting off), or
- `GZIPOutputStream.finish()`/header-trailer handling, or
- a `ByteBuffer`/stream-position bug in the kafka `CompressionType.GZIP` wrapper.

Reproduce with a tiny GZIP round-trip (`GZIPOutputStream` → bytes →
`GZIPInputStream`) and a kafka `MemoryRecordsBuilder` GZIP round-trip under CratonVM.

## Affected classes (partial — append more later)
- producer.internals.ProducerBatchTest
- (expect common.record / common.compress MemoryRecords GZIP tests too — append
  from the full run)
