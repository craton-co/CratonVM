# `EOFException: Unexpected EOF` from CratonVM's `ByteBuffersDataInput` natives during Lucene LZ4 preset-dict compression

## Status
**OPEN** — reproduced on unmodified `dev` with a 40-line Lucene-only probe.
Found 2026-07-31 behind the `Files.newInputStream` fix
(`docs/internal/bug-files-newinputstream-slurps-character-devices-FIXED-20260731.md`);
with that landed, H2's `FULLTEXT_LUCENE` reaches Lucene's index writer and this
is what stops it.

## Severity
**MEDIUM** — blocks every Lucene index *write*. Reading works. The two H2
full-text test classes below cannot pass until this is fixed; Elasticsearch
indexing paths are likely affected by the same shim.

## Affected test classes (H2 suite)
- `org.h2.test.db.TestFullText` (`testCreateDropLucene`)
- `org.h2.test.unit.TestRecovery` (`testRecoverFulltext`)

Both now fail with `exit=1` and this exception (they used to be OOM-killed
before reaching it).

## Symptom
```
java/io/EOFException: Unexpected EOF
  at org/apache/lucene/codecs/lucene90/LZ4WithPresetDictCompressionMode$LZ4WithPresetDictCompressor.compress(LZ4WithPresetDictCompressionMode.java:181)
  at org/apache/lucene/codecs/lucene90/compressing/Lucene90CompressingStoredFieldsWriter.flush(Lucene90CompressingStoredFieldsWriter.java:259)
  at org/apache/lucene/codecs/lucene90/compressing/Lucene90CompressingStoredFieldsWriter.finish(Lucene90CompressingStoredFieldsWriter.java:462)
  at org/apache/lucene/index/StoredFieldsConsumer.flush(StoredFieldsConsumer.java:104)
  at org/apache/lucene/index/IndexingChain.flush(IndexingChain.java:282)
  at org/apache/lucene/index/DocumentsWriterPerThread.flush(DocumentsWriterPerThread.java:392)
  ...
  at org/apache/lucene/index/IndexWriter.commit(IndexWriter.java:4023)
```

The message is **CratonVM's own**, not Lucene's: `lucene_eof()` in
`native-builtins/src/lucene_es.rs:1882`. Lucene's `ByteBuffersDataInput` is
reimplemented natively there, and one of its ~16 `lucene_eof()` bail-outs is
firing where the real class would have returned data.

`LZ4WithPresetDictCompressor.compress` reads a dictionary prefix and then
successive sub-blocks out of a `ByteBuffersDataInput`, seeking between them —
i.e. it exercises the absolute-position (`readBytes(long pos, ...)`) and
sequential paths together, plus the block-boundary arithmetic in
`lucene_bbdin_copy_abs_to_array` (`blockBits` / `blockMask` / per-block
`offset`+`limit`). That combination is the obvious place to look first.

## Repro (Lucene only, no H2)
`docs/known-issues/repros/newinputstream/LuceneMini.java` — open an
`FSDirectory`, add one `TextField` document, `commit()`.

```bash
<cratonvm> --java-home /home/victor/jdk25 --Xmx 512m --nojit \
  -Dtests.seed=deadbeef -c "$(cat apps/h2database/h2/craton-testcp.txt):." LuceneMini
```

`-Dtests.seed=deadbeef` matters on a binary that predates the
`Files.newInputStream` fix: it makes Lucene's `StringHelper.<clinit>` skip its
`/dev/urandom` read, which would otherwise OOM-kill the process first. It is
also what proves this bug is independent of that fix — with the flag set, the
unmodified `dev` binary reaches the identical exception at the identical point
with identical RSS (280 MB).

Expected (real HotSpot 25.0.3): the probe prints `DONE` with
`DirectoryReader.open numDocs=1`.

## Where to start
1. Instrument each `lucene_eof()` site in `lucene_es.rs` with a distinct
   message (or a `tracing::debug!`) and re-run the probe — there are 16 of
   them and the current message says nothing about which fired.
2. Compare against the real `org.apache.lucene.store.ByteBuffersDataInput`
   semantics for the failing call. Note `lucene_bbdin_copy_abs_to_array`
   treats `block_offset >= limit` as EOF *mid-copy*, which is wrong if a read
   legitimately spans into the next block whose data starts at that block's
   own `offset`.
3. Check whether disabling the native shim (letting the real Lucene bytecode
   run) makes the probe pass — that isolates shim-vs-VM immediately.
