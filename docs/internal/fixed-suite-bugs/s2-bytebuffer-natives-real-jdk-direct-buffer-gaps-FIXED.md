# s2 ByteBuffer natives: real-JDK direct-buffer / interop gaps — FIXED

Status: **FIXED (2026-07-13)**, branch `fix/s2-bytebuffer-direct-gaps-20260713`
(originally split out of
`docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object-FIXED.md`)

All items below were closed in one pass on the Azure host (worktree
`/data/wt-s2bb-gaps-20260713`). Root causes, fixes, and verification per
item; the fix follows the doc's own "single storage-view helper" suggestion
(`servlet.rs::s2_bb_storage` — heap array + real `offset` base OR native
address — plus `s2_bb_read_window`).

## The 2026-07-13 compound-file finding: NOT a dev bug — observed on uncommitted local changes

The "misplaced codec footer (file truncated?): remaining=-30, expected=16,
fp=2517" failure recorded here on 2026-07-13 **does not reproduce on the dev
tip**. The exact forced-NIOFSDirectory
`ES93FlatBFloat16VectorFormatTests.testMultiClose` repro (seed
`B17AC9D3E1F2A0C4`, `-Dtests.directory=NIOFSDirectory`,
`-Dtests.method=testMultiClose`) **passes on a clean dev build** and passes
on the fixed build (multiple runs, see verification below).

The failure — including the earlier `DirectBuffer.cleaner().clean()` NPE
stage the note describes being "moved past" — reproduces only with the
**uncommitted ~460-line diff** left in worktree
`/data/data/cratonvm-s2-bytebuffer-real-direct-20260712` (branch
`codex/fix-s2-bytebuffer-real-direct-20260712`, never merged): running that
session's binary (`cratonvm-s2-bytebuffer-20260712`) against the identical
repro command reproduces `remaining=-30, expected=16, fp=2517` exactly,
while the dev-tip binary passes. That diff reworks `ByteBuffer.allocateDirect`
to return abstract-stamped synthetic buffers (instead of dev's real
`DirectByteBuffer` via `new_object_initialized`), which pushes every direct
buffer through JDK I/O paths (`IOUtil.write`'s `instanceof DirectBuffer`
check, `Util` temp-buffer cache, `(DirectBuffer)` casts) that expect a real
`DirectByteBuffer` — the byte loss is an artifact of that rework, not of
dev. No dev-side fix is required for this section; do not re-apply that
worktree's allocateDirect rework without revisiting this.

## Item-by-item

1. **Bulk `get([BII)`/`get([B)`/`put([BII)`/`put([B)` on DIRECT receivers** —
   already FIXED 2026-07-12 (prior session, merged). Unchanged; this pass
   added real-`offset` (array-base) support to the same copy loops for
   aliasing heap views (see item 3).

2. **`equals` / `hashCode` / `compareTo` heap-array-only** — FIXED. Now
   storage-view based, so direct receivers (including genuine real-JDK
   `DirectByteBuffer`s — these methods resolve on the abstract class and
   are force-dispatched) and mixed heap/direct comparisons work. Also fixed
   in the same pass: `hashCode` iterated FORWARD while real
   `Buffer.hashCode` iterates BACKWARD (`for (i = limit()-1; i >= position(); i--)`)
   — the s2 value differed from HotSpot for every buffer with 2+ remaining
   bytes. Verified byte-for-byte against HotSpot (probe `hashRef`).

3. **`compact()` / `slice()` / `slice(II)` (newly registered) /
   `duplicate()` / `asReadOnlyBuffer()` / `asXxxBuffer()` views** — FIXED.
   All are now ALIASING views sharing the parent's storage, exactly like
   real-JDK buffers: heap views record an array-base `offset` (honoured by
   every accessor via `s2_bb_heap_base`; `arrayOffset()` now returns it),
   direct views record the advanced native `address`. The old `slice()`
   copied into a fresh array (writes through a slice never reached the
   parent — silent data divergence) and returned all-zero for direct
   sources; `duplicate()`/`asReadOnlyBuffer()` on a direct source returned
   a buffer with NO storage at all. Typed views (`asIntBuffer()` etc. plus
   their `slice`/`slice(II)`/`duplicate`) now alias direct storage too, and
   fold a sliced parent's base into the view's byte-start.
   `isDirect()`/`isReadOnly()`/`hasArray()`/`array()` now answer from the
   storage/flags (previously hardcoded heap-and-writable); mutating ops on
   read-only buffers throw the new `ReadOnlyBufferException`.

4. **`ChecksumIndexInput.getChecksum()` diverges from HotSpot** — FIXED,
   and it was NOT in the ByteBuffer family at all: CratonVM registers a
   native override for
   `org/apache/lucene/store/BufferedChecksumIndexInput.getChecksum()J`
   (native-builtins/src/phases_late.rs) which, whenever the input's
   position was within 8 bytes of EOF, RE-READ the file and recomputed a
   CRC over `length - 8` bytes — a synthetic-era workaround shaped around
   CodecUtil's footer idiom (where position == length-8 at the call, so
   the heuristic coincided with the right answer — which is why Lucene's
   own write-then-verify cycles always passed). Any other caller shape got
   the wrong window: reading the ENTIRE 5000-byte file yielded
   CRC(bytes[0..4992]) = 170114997 instead of CRC(all read) = 2329538857 —
   exactly the recorded divergence (verified arithmetically). The digest
   path (Lucene `BufferedChecksum` over `java.util.zip.CRC32`) is fully
   correct under CratonVM (probed: bulk/per-byte/chunked/interface-dispatch
   /ByteBuffer-update, plus mixed readInt/readLong/skipBytes through
   `openChecksumInput`), so the native now simply returns
   `digest.getValue()` — mirroring the real Lucene bytecode — and the
   redundant near-EOF full-file re-read is gone.

5. **Reflective `Directory` reconstruction `NoSuchMethodException: <init>`
   under `-Dtests.directory=ByteBuffersDirectory`** — FIXED, also not a
   ByteBuffer bug: `Class.asSubclass` (native_class_as_subclass,
   native-builtins/src/lang_class.rs) unconditionally returned `this`,
   never throwing `ClassCastException`. Lucene's
   `LuceneTestCase.newFSDirectory` deliberately relies on
   `CommandLineUtil.loadFSDirectoryClass(TEST_DIRECTORY)` throwing CCE for
   a non-FSDirectory class to fall back to a random FSDirectory; with the
   lenient asSubclass it proceeded to
   `ByteBuffersDirectory.getConstructor(Path.class, LockFactory.class)` —
   no such ctor — and the test failed with an uncaught
   `NoSuchMethodException: <init>`. asSubclass now enforces the subtype
   check (via the existing isAssignableFrom native, arrays/interfaces
   included) and throws HotSpot-style CCE.

6. **`ByteBuffer.order()` getter round-trip display quirk** — FIXED. Three
   related causes, all in the ByteOrder natives, none in the buffer state
   (which was already correct): (a) `ByteOrder.toString`/`equals` s2
   natives decoded field 0 as an order int — on REAL ByteOrder objects
   field 0 is the `name` String, so `LITTLE_ENDIAN.toString()` printed
   "BIG_ENDIAN"; now layout-aware. (b) `ByteOrder.nativeOrder()` (and the
   BIG_ENDIAN/LITTLE_ENDIAN static getters) allocated a FRESH synthetic
   per call — `nativeOrder() == ByteOrder.LITTLE_ENDIAN` was always false,
   and in real-JDK mode the order int corrupted the real 1-field layout's
   name slot; now they return the canonical real statics. (c) the
   `buffer.order()` getter's static lookup silently fell back to a fresh
   synthetic when it ran before `ByteOrder.<clinit>`; the shared
   `s2_byte_order_object` helper now ensures initialization first. Typed
   views' `order()` used the same fresh-synthetic pattern and now share
   the helper.

7. **`ES93FlatBFloat16VectorFormatTests` 7-vs-53 iterations under JIT** —
   re-verified on the fixed build with the original seed: 53/53 tests pass
   under BOTH JIT-on and `--nojit` (matching HotSpot). The divergence does
   not reproduce; closed alongside the rest of this doc.

## Verification (Azure host, dev tip + this fix)

- `ProbeBBSemantics` (heap/wrap/direct slice-dup-asRO aliasing, compact,
  read-only semantics, order round-trips, equals/hashCode/compareTo mixed
  heap/direct, JDK-reference hashCode): ALL-PASS under the fixed build;
  byte-identical expectations validated against HotSpot (jdk25).
- `ProbeCRC`/`ProbeCRC2`/`ProbeCRC3` (CRC32 paths; Lucene BufferedChecksum
  composites; openChecksumInput bulk/per-byte/mixed-readInt-readLong-skip;
  writer-side getChecksum; footer idiom): all values match HotSpot,
  including the footer-idiom value (170114997 over the first 4992 bytes)
  and the full-read value (2329538857).
- `ProbeNIOFS2` (the doc's original item-4 repro): MATCH=true.
- `ProbeOrderDetail`, `ProbeAsSubclass`, `ProbeReflectDir`: match HotSpot.
- Exact `testMultiClose` repro: PASS under forced NIOFSDirectory (multiple
  runs) and under forced ByteBuffersDirectory (item 5's face).
- Full `ES93FlatBFloat16VectorFormatTests`: 53/53 PASS, JIT-on and
  `--nojit`.
- Regression sweep over neighbouring ES vector-codec classes: see commit
  message.

---

## Original doc content (as of retirement, for history)

## s2 ByteBuffer natives: remaining real-JDK direct-buffer / interop gaps

Status: OPEN (residuals split out of
`docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object-FIXED.md`)

### 2026-07-13 focused real-JDK NIOFS finding

The direct-buffer compatibility fixes are sufficient for the focused
real-JDK direct-buffer probes (including a non-null `DirectBuffer.cleaner`,
direct `FileChannel` I/O, `ByteBuffersDirectory` reflection + I/O, byte-order
round trips, and the exact `ChecksumIndexInput.getChecksum()` value).  They
also move the forced `NIOFSDirectory`
`BaseIndexFileFormatTestCase.testMultiClose` repro past its former
`DirectBuffer.cleaner().clean()` null-pointer failure.

The same deterministic repro is **still open**: it now ends with
`CorruptIndexException: misplaced codec footer (file truncated?)`, reporting
`remaining=-30, expected=16, fp=2517`.  System-call tracing records only a
2479-byte body write followed by an 8-byte write, while Lucene's logical
output position expects a 2533-byte file: 46 bytes are lost before the final
OS write, not by a later read-back or footer check.  Standalone direct
`FileChannel` (including a 131071-byte stress probe), transfer, and
`OutputStreamIndexOutput` alignment probes preserve their physical lengths,
so this is not a generic direct-buffer allocation, cleaner, file-channel, or
alignment failure.

The remaining evidence points at the compound-file `copyBytes` path:
`Lucene90CompoundFormat.writeCompoundFile` reports its logical file pointer
after copying input through the force-dispatched ByteBuffer family, but the
produced compound body is short.  In particular, the forced
`ByteBuffer.get([BII)` / `put([BII)` dispatch reached by the input/output
bulk-copy path remains the next narrow runtime boundary to instrument with
the exact compound-file chunk sizes.  Do not retire this note until that
copy-path pointer/physical-length divergence is eliminated and the exact
`testMultiClose` command passes.

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
