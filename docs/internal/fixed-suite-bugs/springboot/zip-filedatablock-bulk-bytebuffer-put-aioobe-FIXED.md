# `FileDataBlock$FileAccess.read`: bulk `ByteBuffer.put(ByteBuffer)` throws `ArrayIndexOutOfBoundsException` — FIXED 2026-07-12

**Status: FIXED, merged to `dev`.** Was: OPEN, characterized. Severity was
HIGH (universal within the affected module — not an edge case): every class
in `loader/spring-boot-loader` failed (`ZipContentTests`, `NestedJarFileTests`,
`SecurityInfoTests`, `UrlJarFileFactoryTests`, `UrlNestedJarFileTests`,
`NestedUrlConnectionTests`, `NestedPathTests`, `NestedFileSystemProviderTests`,
`FileDataBlockTests`, `VirtualZipDataBlockTests`), including trivial methods
like `sizeReturnsNumberOfEntries`/`getCommentReturnsComment`, on every bulk
`ByteBuffer.put(int, ByteBuffer, int, int)` (via `ScopedMemoryAccess.copyMemory`
→ `Unsafe.copyMemory`).

## Root cause

**Not** a bounds-check bug in `native_unsafe_copy_memory_consolidated` (the
function this doc originally suspected, per the "likely connection to prior
work" section below — that function is correct and unrelated). The real bug
was in `native-builtins/src/servlet.rs::bb_write_hb` (backing
`ByteBuffer.allocate(int)`/`wrap(...)` via `s2_bb_alloc`), which is the LIVE
default-release dispatch site for these methods (confirmed empirically —
`native_heap_bytebuffer_allocate` in `lib.rs`, previously believed to be the
live allocator per `bytebuffer-address-unset-aioobe.md`, is NOT reached for
plain `ByteBuffer.allocate()`/`wrap()`; `s2_bb_alloc` wins).

`bb_write_hb` wrote BOTH the correct real-JDK named fields (`hb`, `position`,
`limit`, `capacity`, `mark`, ...) AND a legacy "synthetic-mode indexed
fallback" (`BB_ARRAY`=0, `BB_POS`=1, `BB_LIMIT`=2, `BB_CAP`=3, `BB_MARK`=4,
`BB_ORDER`=5) — unconditionally, running AFTER the by-name writes. On a
real-JDK-shaped object, real `java.nio.Buffer`'s field layout is
`mark@0/position@1/limit@2/capacity@3/address@4/segment@5`. So:

- `set_field(buf, BB_MARK=4, Int(-1))` clobbered the real `address` field
  (needed by every bulk `get`/`put` via `ScopedMemoryAccess.copyMemory`) with
  `-1`, instead of the `16` (`ARRAY_BYTE_BASE_OFFSET`) the by-name write had
  just set.
- `set_field(buf, BB_ARRAY=0, Object(arr))` clobbered the real `mark` field
  with a coerced, truncated array-pointer int.

`ByteBuffer.putBuffer` computes the destination native-copy offset as
`address + position`; with `address == -1`, `Unsafe.copyMemory`'s
destination-bounds check (`byte_off.checked_sub(ABASE=16)`) underflowed on
every call, throwing `ArrayIndexOutOfBoundsException` — universally, on the
very first bulk `put`/`get`, matching the "28/29 methods fail, even trivial
ones" symptom exactly.

This is the same collision class the 2026-07-11 typed-buffer-view fix
(`s2_bb_synthetic_layout`, see the `BB_SEGMENT_SLOT` comment in the same
file) already found and fixed for `s2_bb_order`/`s2_bb_set_order`/`s2_bb_arr`
— `bb_write_hb` itself just wasn't updated to use the same discriminator.

## Fix

`native-builtins/src/servlet.rs::bb_write_hb`:
- Added the missing `ctx.set_field_by_name(buf, "address", Value::Long(16))`
  write (mirrors the `alloc_heap_bytebuffer`/
  `native_heap_byte_buffer_init_array_offset_len` fix from
  `bytebuffer-address-unset-aioobe.md`, which never applied to this path).
- Gated the legacy indexed-fallback block behind the existing
  `s2_bb_synthetic_layout(ctx, buf)` discriminator, so it only runs for the
  genuinely-synthetic (non-real-JDK) layout, not a real `ByteBuffer` whose
  by-name writes already did the job correctly.

Also fixed, found in the same investigation: `jdk/internal/misc/ScopedMemoryAccess`'s
`closeScope0`/`copyMemory`/`copyMemoryInternal` were registered under the
stale JDK21-era descriptor `Ljdk/internal/misc/ScopedMemoryAccess$Scope;`
instead of JDK 25's `Ljdk/internal/foreign/MemorySessionImpl;` (every sibling
registration in the same block — `getByte`/`putByte`/etc. — already used the
correct descriptor). This made the registration permanently dead (an exact
string-keyed registry lookup never matches), harmless only because real
`copyMemoryInternal` bytecode falls through to the correctly-registered
`Unsafe.copyMemory`. Fixed the descriptor to match.

## Verification

- Minimal standalone repro (heap `ByteBuffer.allocate(64).put(0, directBuf, 0,
  32)`, mirroring `FileDataBlock$FileAccess.read`'s exact call shape):
  reproduced the AIOOBE byte-for-byte against the pre-fix binary (`address`
  read back as `-1`, `mark` read back as a truncated array pointer via
  reflection), passes cleanly after the fix (`address=16`, `mark=-1`).
- New unit tests `bb_write_hb_real_layout_preserves_address_and_mark` /
  `bb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback` in
  `native-builtins/src/servlet.rs`.
- `cargo test -p cratonvm-native-builtins --lib`: same 6 pre-existing,
  unrelated failures before and after the fix (JCA Ed25519, jspecify
  type-use annotations ×2, xerces whitespace-normalize, and this doc's own
  now-superseded `bytebuffer_allocate_initializes_real_address_for_bulk_copy`
  test which exercises the *dead* `native_heap_bytebuffer_allocate` path
  directly and was already failing pre-fix for an unrelated reason) — zero
  regressions.
- Re-ran the `loader/spring-boot-loader` module (54 classes) against the
  fixed binary: **33 PASS / 15 FAIL / 2 EMPTY / 1 HANG** at the default 300s
  per-class timeout, up from near-total failure (previously 9/9 classes
  universally broken by this bug). `FileDataBlockTests` itself: 12/13 PASS
  (was 0/13).
- The one "HANG" (`ZipContentTests`) was re-run standalone with a 1200s
  timeout: it completed in 957s (~16 min) with **27/29 tests PASS** — not a
  hang, just a legitimately slow class (see the Zip64 residual below). The
  2 remaining failures are unrelated to `copyMemory`/`ByteBuffer` (a
  zip-entry-timestamp mismatch and a `java.util.zip.ZipFile` lifecycle NPE).

## Known residuals (separate, unrelated bugs — NOT part of this fix)

Re-running the module surfaced pre-existing, differently-rooted bugs that
this AIOOBE had been masking (none reference `copyMemory`/`ScopedMemoryAccess`/
`ArrayIndexOutOfBoundsException` in their failure output):

- `MetaInfVersionsInfoTests` — multi-release JAR version-entry parsing finds
  an unexpected extra element (`[9]`).
- `ExplodedArchiveTests.getClassPathUrlsWhenNoPredicatesReturnsUrls` —
  classpath URL set omits several expected entries (manifest, nested data
  files, a percent-encoded filename).
- `SecurityInfoTests`/`NestedJarFileTests` (signed-jar cases) — jar-signature
  verification (`SecurityInfo.load`) fails, and a `bcprov-jdk18on` jar is
  left open past test teardown (`AssertFileChannelDataBlocksClosedExtension`
  file-handle leak).
- `ZipContentTests.openWhenZip64ThatExceedsZipSizeLimitOpensZip` — writes
  6× 1 GiB (6 GiB total) through `ZipOutputStream`/`FileInputStream.transferTo`
  to force a real Zip64 central directory; takes ~16 minutes wall-clock on
  CratonVM (confirmed via a standalone 1200s-timeout run — completes, not an
  infinite loop), so it alone exceeded the runner's default 300s per-class
  timeout. A CratonVM-vs-HotSpot throughput characteristic, not a
  correctness bug — not investigated further.
- `ZipContentTests.getEntryAsCreatesCompatibleEntries` — a zip entry's
  `getLastModifiedTime()` returns what looks like the current wall-clock
  time (`1783891982000L`ms) instead of the timestamp actually stored in the
  zip's DOS-date fields (`315446416000L`ms) — a real-JDK zip-entry
  date/time decoding bug, unrelated to `copyMemory`.
- `ZipContentTests.getDataWhenNestedDirectoryReturnsVirtualZipDataBlock` —
  `java.util.zip.ZipFile.stream()`/`ensureOpen()` NPEs on `this.res` being
  null — a `java.util.zip.ZipFile` lifecycle/field-initialization bug,
  unrelated to `copyMemory`.

These are out of scope for this doc (each is an independent root cause in a
different subsystem — jar signature verification, file-handle lifecycle,
multi-release jar parsing, URL enumeration) and were not chased further here.

## Original repro (still valid, now passes)

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for loader/spring-boot-loader's org.springframework.boot.loader.zip.ZipContentTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```

## Original "likely connection to prior work" section (for the record — turned out NOT to be the cause)

This looked like the same call shape (`ScopedMemoryAccess.copyMemory` between
a direct/arena buffer and a heap buffer) as the already-fixed "mixed
heap↔off-heap `Unsafe.copyMemory` silently dropped" bug documented in memory
under `reference_server_socket_gap` (UPDATE 4,
`native_unsafe_copy_memory_consolidated` in
`native-builtins/src/unsafe_natives.rs`). That function was investigated in
detail and found to be correct for this call shape (heap-dst/off-heap-src);
the actual bug was one layer up, in how the destination `ByteBuffer` object's
fields were initialized before the copy ever ran.
