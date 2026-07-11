# s2 ByteBuffer natives: remaining real-JDK direct-buffer / interop gaps

Status: OPEN (residuals split out of
`docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object-FIXED.md`)

Context: the s2 synthetic ByteBuffer native family
(`native-builtins/src/servlet.rs`, sibling copy in `tests_extracted.rs`) is
force-dispatched over real JDK bytecode for ~60 `java/nio/ByteBuffer`
methods (`interpreter.rs::force_native_over_real_jdk_bytecode`). It was
written for the 6-slot synthetic layout (`array, pos, limit, cap, mark,
order`) and has been retrofitted for real-JDK buffer objects
piece-by-piece (named `hb` in `s2_bb_arr`, named `mark` in
`s2_bb_get/set_mark`, direct-buffer `address` + named `bigEndian` in
`put(ByteBuffer)`/`s2_bb_order` — the 2026-07-10 vector-codec family fix).
The following gaps remain, found by code inspection during that fix (none
currently tied to a failing suite row):

1. **Bulk `get([BII)` / `get([B)` on a DIRECT receiver read from the
   destination array.** Both use `s2_bb_arr(ctx, this).unwrap_or(dst)` — a
   direct buffer has no heap array, so the copy source silently becomes the
   destination itself (garbage read, position still advanced). Same
   direct-storage handling as the fixed `put(Ljava/nio/ByteBuffer;)` is
   needed. The bulk `put([BII)`/`put([B)` write side has the same shape
   (silently drops the write into `unwrap_or(src)`).
2. **`equals` / `hashCode` / `compareTo` are heap-array-only** — a direct
   buffer compares as empty/zero.
3. **`compact()`, `slice()`, `duplicate()`, `asReadOnlyBuffer()` and the
   `asIntBuffer()`-style views copy through `s2_bb_arr` only** — direct
   receivers produce empty results. The views also copy the raw `BB_ARRAY`
   slot from a possibly-real source (slot 0 = real `mark`), so views over
   real heap buffers are suspect too.
4. **`ChecksumIndexInput.getChecksum()` value diverges from HotSpot** on a
   byte-identical NIOFS file (probe `/tmp/ProbeNIOFS.java` on the Azure
   host: CratonVM `98c9b84c` vs HotSpot `32939d57` for the same 5000
   bytes; `java.util.zip.CRC32` itself verified byte-for-byte correct via
   `/tmp/ProbeCrc.java`). Self-consistent within one VM, so Lucene's own
   write-then-verify cycles pass; only cross-VM index interchange would
   notice. Suspect the `BufferedChecksumIndexInput` update path (checksum
   updated over a different byte window than HotSpot, e.g. buffered vs
   re-read bytes).
5. **Reflective `Directory` reconstruction gap**: forcing
   `-Dtests.directory=ByteBuffersDirectory`,
   `BaseIndexFileFormatTestCase.testMultiClose` fails with
   `java.lang.NoSuchMethodException: <init>` (the test rebuilds the
   directory class reflectively). Not vector-specific; only under a forced
   directory.

6. **`ByteBuffer.order()` getter round-trip display quirk**: after the
   2026-07-10 fix, all multi-byte data paths honor `order(LITTLE_ENDIAN)`
   (verified: LE `putInt` byte layout, LE `getInt`/`getShort`/`getLong`,
   Lucene `BufferedIndexInput` reads), but `buffer.order()` still returns an
   object printing as `BIG_ENDIAN` after `order(LITTLE_ENDIAN)`
   (`/tmp/ProbeOrder.java`). Real code branching on
   `order() == ByteOrder.LITTLE_ENDIAN` may take the wrong branch even
   though reads/writes are correct — the getter's real-static lookup or a
   competing `order()` registration needs a follow-up look.
7. **`ES93FlatBFloat16VectorFormatTests` runs 7 test iterations under
   JIT-on vs 53 under `--nojit` and HotSpot** (same seed). All green
   post-fix, but the RandomizedRunner iteration-count divergence under JIT
   is unexplained.

Suggested direction: rather than patching the remaining methods one at a
time, consider giving the s2 family a single storage-view helper (heap
array + offset OR native address) like native-io's `bb_storage_view`, or
consolidating the two families — native-io's implementations already
handle both storages but lose the registration race to the s2 family for
the `java/nio/ByteBuffer` key.
