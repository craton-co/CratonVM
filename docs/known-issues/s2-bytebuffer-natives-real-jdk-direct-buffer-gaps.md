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

1. **FIXED (2026-07-12).** Bulk `get([BII)` / `get([B)` / `put([BII)` /
   `put([B)` on a DIRECT receiver used `s2_bb_arr(ctx, this).unwrap_or(dst/src)`
   — a direct buffer has no heap array, so the copy source/destination
   silently became the destination/source array itself (self-copy: writes
   dropped, reads returned whatever the caller's array already held).
   This was the confirmed root cause of
   `docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-vector-codec-footer-mismatch-FIXED.md`
   (Lucene footer/checksum bytes reading back as zero via `IOUtil.read`'s
   temp-direct-buffer + bulk-`get` pattern). Fixed by adding the same
   `s2_bb_direct_addr` + `copy_from/to_native_memory` fallback that
   `put(Ljava/nio/ByteBuffer;)` already had, throwing
   `IllegalStateException` on a failed native-memory access (matching that
   fix's pattern).

   **Related, also fixed in the same pass:** `s2_bb_get_byte`/
   `s2_bb_put_byte` — the single-byte primitives every multi-byte
   `read2/4/8`/`write2/4/8` helper funnels through — had the identical gap
   (only checked `s2_bb_arr`, silently read 0 / dropped the write
   otherwise). Verified this does NOT affect scalar/typed accessors
   (`get(int)`, `getInt`, `getLong`, etc.) on a genuine real-JDK
   `DirectByteBuffer`: those methods are overridden by the *concrete*
   `DirectByteBuffer` class and run as real bytecode (using the buffer's
   real `address` field), bypassing this native family's force-dispatch
   entirely — confirmed via a debug-instrumented build showing the
   registered closures are never entered for `ByteBuffer.allocateDirect(...)`
   objects. The gap DOES matter for anything that stays inside the s2
   family without a `DirectByteBuffer`-shaped concrete class backing it —
   slices/duplicates of a direct buffer (see item 3 below) and typed
   buffer views (`asIntBuffer()` etc., see item 3) built over one — so
   `get_byte`/`put_byte` were fixed defensively with the same fallback
   (panic-free, benign 0/no-op on a failed native-memory access, since
   these low-level helpers have no `MethodCallResult` to throw through).

2. **`equals` / `hashCode` / `compareTo` are heap-array-only** — a direct
   buffer compares as empty/zero.
3. **`compact()`, `slice()`, `duplicate()`, `asReadOnlyBuffer()` and the
   `asIntBuffer()`-style views copy through `s2_bb_arr` only** — direct
   receivers produce empty results. Confirmed still open (not touched by
   the 2026-07-12 fix): `slice()` on a plain `ByteBuffer` unconditionally
   allocates a NEW heap array and only copies into it
   `if let Some(src) = s2_bb_arr(ctx, this)` — for a direct source that
   condition is false, so the slice silently comes back all-zero instead
   of aliasing the direct storage. The views also copy the raw `BB_ARRAY`
   slot from a possibly-real source (slot 0 = real `mark`), so views over
   real heap buffers are suspect too.
4. **`ChecksumIndexInput.getChecksum()` value diverges from HotSpot** on a
   byte-identical NIOFS file. Reconfirmed 2026-07-12 with a fresh probe
   (`ProbeNIOFS2.java`: write 5000 bytes via `NIOFSDirectory`, read back
   through `openChecksumInput`) on the current dev tip, both before and
   after the item-1 fix above: CratonVM consistently returns `170114997`
   vs HotSpot's `2329538857` for identical on-disk bytes (verified
   byte-identical: `readBackMatches=true`). **The item-1 ByteBuffer bulk
   fix does NOT resolve this** — same wrong checksum value pre- and
   post-fix, confirming this is a separate root cause, most likely in the
   `BufferedChecksumIndexInput` update path as originally suspected
   (checksum updated over a different byte window than HotSpot, e.g.
   buffered vs re-read bytes) rather than in the raw `ByteBuffer`
   accessors. Self-consistent within one VM, so Lucene's own
   write-then-verify cycles pass; only cross-VM index interchange would
   notice.
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
   JIT-on vs 53 under `--nojit`** (same seed). Re-verified 2026-07-12 as
   part of the item-1 fix validation: the full class now runs 53/53 under
   JIT-on too (matching `--nojit` and HotSpot) with the current dev tip +
   fix, so this divergence no longer reproduces — leaving open pending a
   dedicated re-check with the exact original repro conditions, since it's
   unclear whether item 1 incidentally fixed the iteration-count
   divergence or whether it was already gone for an unrelated reason.

Suggested direction: rather than patching the remaining methods one at a
time, consider giving the s2 family a single storage-view helper (heap
array + offset OR native address) like native-io's `bb_storage_view`, or
consolidating the two families — native-io's implementations already
handle both storages but lose the registration race to the s2 family for
the `java/nio/ByteBuffer` key **only when the `synthetic-jdk` cargo feature
is enabled** (`register_nio_natives` in `native-io/src/lib.rs` is gated
`#[cfg(feature = "synthetic-jdk")]` and is not compiled into a default
real-JDK-mode build at all, so on a default build there is no race — s2 is
simply the only registrant).
