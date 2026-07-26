# ES FAIL family - Build current holder receives null Manifest - FIXED 2026-07-10

| | |
|---|---|
| **Status** | FIXED 2026-07-10. Root-caused to a VM-core bug (real-JDK-mode bootstrap ordering leaves `jdk/internal/misc/Unsafe`'s `ARRAY_*_BASE_OFFSET`/`ARRAY_*_INDEX_SCALE` static constants at 0), not anything ES-specific or manifest/classloading-specific. |
| **Area** | VM core — `jdk/internal/misc/Unsafe.<clinit>` post-clinit backfill (`../../../../vm/src/vm/vm_util.rs`). |
| **Symptom** | Every `Build.current()` call under real-JDK mode (`--java-home`) threw `ExceptionInInitializerError` from `Build$CurrentHolder`, caused by `NullPointerException: Cannot invoke "java.util.jar.Manifest.getMainAttributes()" because "manifest" is null`, blocking 265 ES test-suite rows at bootstrap. |
| **Severity** | high — blocked the entire affected family before any test logic ran. |

## Original symptom (2026-07-10, local Windows box)

- Run: `esfull-20260710-083851`
- Family count: 265 rows in the stopped partial run, all sharing the same
  `ExceptionInInitializerError`/`NullPointerException` shape from
  `Build$CurrentHolder.findCurrent()` → `Build.findLocalBuild()`.
- Representative class: `libs/cli-terminal org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests`.
- HotSpot passed the same representative class with the identical classpath.

## Root cause

`Build.findLocalBuild()` correctly resolves `Build.class`'s `CodeSource` to
the real `elasticsearch-<version>.jar` (this part of the code-source/
protection-domain machinery, `../../../../native-builtins/src/lib.rs`'s
`java/lang/Class.getProtectionDomain()` native, was verified correct via a
byte-level A/B probe against HotSpot — not the bug, despite being the first
suspect from the original symptom). It then does
`new JarInputStream(FileSystemUtils.openFileURLStream(url)).getManifest()` —
real JDK bytecode, not natively overridden. The bytes handed to that
`JarInputStream` are also byte-for-byte correct (verified: `PK\x03\x04...`,
full file length, matches HotSpot's own read of the same file).

The actual defect is one level deeper: `ZipInputStream.getNextEntry()`'s
`readLOC()` calls `java.util.zip.ZipUtils.get32(byte[], int)` /
`get16(byte[], int)`, which are implemented as:

```java
return Integer.toUnsignedLong(
    UNSAFE.getIntUnaligned(b, off + Unsafe.ARRAY_BYTE_BASE_OFFSET, false));
```

`jdk.internal.misc.Unsafe.ARRAY_BYTE_BASE_OFFSET` (and its 8 siblings —
`ARRAY_BOOLEAN_BASE_OFFSET` through `ARRAY_OBJECT_BASE_OFFSET`, plus the 9
`ARRAY_*_INDEX_SCALE` fields) are `static final` fields computed once, in
`Unsafe.<clinit>`, by calling `theUnsafe.arrayBaseOffset(byte[].class)` →
the private native `arrayBaseOffset0(Class)`. CratonVM's native registry
*does* correctly implement `arrayBaseOffset0`/`arrayIndexScale0` (both
registered under `jdk/internal/misc/Unsafe`, returning the same constant
`16` this VM uses everywhere else — confirmed by calling
`Unsafe.arrayBaseOffset(byte[].class)` explicitly later in the same process,
which correctly returns `16`). But `Unsafe`'s own `<clinit>` runs earlier in
real-JDK-mode bootstrap than the native registration pass that wires up
`arrayBaseOffset0`/`arrayIndexScale0` — so at the moment those 18 static
fields are computed, the natives are not yet registered, the calls silently
return the zero value for their return type (no exception, no crash), and
the fields are permanently latched at `0` for the rest of the process (they
are `static final`, computed exactly once).

`get32(tmpbuf, 0)` therefore reads
`getIntUnaligned(tmpbuf, 0 + 0, false)` instead of the correct
`getIntUnaligned(tmpbuf, 0 + 16, false)` — 16 bytes short of where the LOC
header actually starts relative to this VM's internal `Unsafe` offset
convention — so `readLOC()`'s signature check (`get32(tmpbuf, 0) != LOCSIG`)
always fails, `getNextEntry()` returns `null` on the very first call, and
`JarInputStream`'s manifest-detection loop (`if (e != null) { ... }`) never
runs — `manifest` stays `null`, silently, with **zero exceptions thrown**
anywhere in the chain. This reproduces for **any** jar (confirmed with both
a hand-built few-KB jar and the real 21 MB `elasticsearch-9.5.0-SNAPSHOT.jar`)
under `--java-home`, and is **not** ES-specific in any way — `Build.java`'s
`new JarInputStream(...).getManifest()` call pattern is simply the first
place in the ES suite that exercises `ZipInputStream.getNextEntry()` against
a real jar early enough in bootstrap to hit the still-unfixed constants.

This is the same class of bug the existing `jdk/internal/misc/UnsafeConstants`
post-clinit fixup (`../../../../vm/src/vm/vm_util.rs`, "FFM/Unsafe fix" comment) already
fixed for `ADDRESS_SIZE0`/`PAGE_SIZE`/`BIG_ENDIAN`/`UNALIGNED_ACCESS`/
`DATA_CACHE_LINE_FLUSH_SIZE` — a different class (`Unsafe` itself, not
`UnsafeConstants`) hitting the identical "real bytecode `<clinit>` calls an
unregistered-at-that-point native, gets 0 back, never throws" shape.

## Fix

`../../../../vm/src/vm/vm_util.rs`: added a `"jdk/internal/misc/Unsafe"` success-path
post-clinit fixup (mirroring the existing `UnsafeConstants` arm exactly —
same trigger site, same `post_clinit_fixup` match statement) that backfills
all 9 `ARRAY_*_BASE_OFFSET` fields to `16` and all 9 `ARRAY_*_INDEX_SCALE`
fields to their correct per-type values (`1/1/2/2/4/8/4/8/8` for
boolean/byte/short/char/int/long/float/double/object), matching the values
`native_unsafe_array_base_offset`/`array_index_scale_for_name`
(`../../../../native-builtins/src/lib.rs`) already use everywhere else in this VM.

## Verification

Byte-level A/B probes against HotSpot (JDK 21 and JDK 25) confirmed each
layer of the chain in isolation before finding the actual defect:

- `Class.getProtectionDomain().getCodeSource().getLocation()` — matches
  HotSpot exactly (correct jar path).
- `URL.openStream()` bytes — byte-for-byte identical to HotSpot for both a
  small hand-built jar and the real 21 MB ES server jar.
- `MethodHandles.byteArrayViewVarHandle(int[].class, LITTLE_ENDIAN)` reads —
  correct (this API path does NOT go through `Unsafe.ARRAY_BYTE_BASE_OFFSET`,
  which is why an initial VarHandle-based probe did not reproduce the bug).
- `jdk.internal.misc.Unsafe.getIntUnaligned(b, 0 + Unsafe.ARRAY_BYTE_BASE_OFFSET, false)`
  — reproduced the exact defect: `ARRAY_BYTE_BASE_OFFSET` printed as `0`
  instead of `16`, and the resulting read returned `0x50` instead of the
  correct `0x04034b50`.

After the fix, on a from-scratch build:

- Minimal standalone repro (`Unsafe.getUnsafe().arrayBaseOffset(byte[].class)`
  read from the cached static field, plus a full `ZipInputStream`/
  `JarInputStream.getManifest()` walk against both a hand-built jar and the
  real ES server jar): manifest reads correctly, matching HotSpot's
  `Change`/`Build-Date` attribute values exactly.
- `Build.current()` (direct call, and via the full nested
  `ESTestCase.<clinit>` → `IndicesModule` → `FeatureFlag` → `Build.current()`
  chain the real suite exercises): no `ExceptionInInitializerError`, correct
  build metadata returned, matching HotSpot's output.
- The representative class
  (`org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests`) run via
  `org.junit.runner.JUnitCore` with the exact `run-elasticsearch-suite.ps1`
  flags (`--java-home`, `--Xmx 2g`, the full `--add-opens` set, JIT on) no
  longer hits this bug at all — the `Build$CurrentHolder` failure is
  completely gone. The class now progresses to genuine test execution and
  fails on a **separate, pre-existing, already-known** bug instead
  (`NoSuchMethodError: StreamReadConstraints$Builder.maxNameLength` — a
  loader-blind `invokestatic` class-constant-resolution defect, previously
  investigated and unrelated to this fix; see
  `docs/known-issues/elasticsearch-suite/ES-xcontent-jackson-streamreadconstraints-loader-blind-invokestatic.md`).
  This is expected: the manifest bug was the *first* thing every affected
  class hit at bootstrap, masking whatever came after it. Fixing it
  necessarily un-masks each class's next failure, which will vary
  class-by-class — the 265-row family needs a fresh full-suite run against a
  fixed binary to re-triage what (if anything) remains per class, which is
  out of scope for this fix.
- `cargo test -p cratonvm-vm --lib vm_util::` — 38/38 pass, including
  `clinit_swallow_recovery_rejects_unrecovered_classes` (still correctly
  asserts `jdk/internal/misc/Unsafe` has no *swallow*-recovery path — this
  fix adds a *success*-path backfill, a different, non-conflicting
  mechanism, exactly like the existing `UnsafeConstants` arm).
- Re-ran the fix with `--java-home` omitted (the previously-passing default
  path) to confirm no regression: manifest still reads correctly, and the
  fixup fires there too (the same 0-vs-16 gap existed in that mode, just not
  on a path that happened to surface visibly before).

## Not duplicates

- Do not split this into one document per affected class. The (up to) 265
  affected rows shared this same bootstrap failure shape; now that it is
  fixed, each row's *next* failure (if any) is a separate, distinct bug and
  should be triaged/filed independently as the suite is re-run.
- Distinct from the older `NativeAccessHolder catch LinkageError` note (the
  warning is logged and execution continues past native-access
  initialization before this failure).
