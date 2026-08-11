> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkJni` passes in the 53/1 run. Entirely in-lane, no out-of-file patch, no residual, and the record itself states no baseline needs re-freezing.
>
> Previous location: `docs/known-issues/jdk-only/L7-adler32-missing-natives.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `java.util.zip.Adler32` had no natives at all — `RJdkJni` died on the third checksum line

**Status:** FIXED in source 2026-08-06 (lane L7, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below.

## The failure

`regression-suite/src/RJdkJni.java` fails in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 runs it to `PASS RJdkJni (35 checks)`. Both CratonVM
arms produce the identical trace, which is the tell that this is an ordinary
Compatible-mode defect and not a strict-mode policy drop — nothing was
*refused*, there was simply nothing registered:

```
CK RJdkJni nativeMeta systemNatives>=4=true reflectHash=true
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode method=java/util/zip/Adler32.updateBytes(I[BII)I
Exception in thread "main" java/lang/UnsatisfiedLinkError: java/util/zip/Adler32.updateBytes(I[BII)I
    at RJdkJni.main(RJdkJni.java:288)
    at RJdkJni.zipNatives(RJdkJni.java:118)
    at java/util/zip/Adler32.update(Adler32.java:79)
```

`RJdkJni.zipNatives` (line 117-119) does:

```java
Checksum adler = new java.util.zip.Adler32();
adler.update("123456789".getBytes(StandardCharsets.UTF_8), 0, 9);
check(adler.getValue() == 0x091E01DEL, "Adler32: " + adler.getValue());
```

HotSpot's line for the same run is `adler=152961502`, i.e. `0x091E01DE`.

## What was missing, and why it was missed

`java.util.zip.Adler32` in JDK 25 is pure Java over a single `int adler` field
(seeded to `1` by the field initialiser), delegating every mutation to a
**static** native that takes the current value and returns the new one:

```java
private static native int update(int adler, int b);
private static native int updateBytes(int adler, byte[] b, int off, int len);
private static native int updateByteBuffer(int adler, long addr, int off, int len);
```

`native-builtins/src/zip_real.rs` already covered the sibling `CRC32` natives
(`update`, `updateBytes0`, `updateByteBuffer0`) and the whole
`Deflater`/`Inflater` family, but had **no `Adler32` rows at all**. The only
`Adler32` registrations in the tree were:

* `native-builtins/src/phases_late/zip_streams.rs:3874`
  (`register_p71_zip_extras`) — the **synthetic-mode** model: instance-level
  `<init>()V`, `update(I)V`, `update([BII)V`, `getValue()J`, `reset()V` against
  a fabricated 1-field `Long` layout. `register_p71_zip_extras` is reached only
  from `phases_late.rs:5798`, which does not run in real-JDK mode.
* `native-builtins/src/phases_early.rs:2829` — `Adler32` appears in the
  `registerNatives()V` → `native_noop` list. Inert here: JDK 25 `Adler32` has
  no `registerNatives` (its static block is `ZipUtils.loadLibrary()`), and that
  function does not run in real-JDK mode either.

So the *synthetic* Adler32 was complete and the *real* one was empty. The trap
is that the two use disjoint descriptors — instance `update([BII)V` versus
static `updateBytes(I[BII)I` — so no amount of grepping for
`java/util/zip/Adler32` shows a gap; every mode-agnostic census reports the
class as "covered".

A second, sharper trap: **`Adler32` is not shaped like `CRC32`.** `CRC32` has a
concrete Java `updateBytes` that range-checks and then calls the private
`updateBytes0` native, so `zip_real.rs` registers both names. `Adler32` puts
the range check in the *public* `update(byte[],int,int)` body and `updateBytes`
**is** the JNI entry point. Registering an `updateBytes0` here by analogy would
have bound nothing.

## What was added

All in `native-builtins/src/zip_real.rs`, in a new `Adler32 natives (real-JDK
mode)` section immediately before the registration function, plus three rows in
`register_zip_real_natives`:

| class | method | descriptor | kind |
| --- | --- | --- | --- |
| `java/util/zip/Adler32` | `update` | `(II)I` | `Bridge` |
| `java/util/zip/Adler32` | `updateBytes` | `(I[BII)I` | `Bridge` |
| `java/util/zip/Adler32` | `updateByteBuffer` | `(IJII)I` | `Bridge` |

`Bridge`, not `SyntheticStub`, because each one implements a method the real
JDK image declares `ACC_NATIVE` — there is no Java body for `--jdk-only` to
fall back to, so dropping them would make the real class file unrunnable. This
is the §1.5 test as the bridge ratchet asks it, and all three land in the
`bridge.acc_native` bucket, so `bridge_without_acc_native` (frozen at 9528,
slack 0) does not move.

The three natives are thin wrappers over `adler32_update(u32, &[u8]) -> u32`,
which already existed at `zip_real.rs:78` as the rolling RFC 1950 §9 update the
`Deflater`/`Inflater` `getAdler` bridges accumulate with. Reusing it rather
than writing a second loop is deliberate: a stream's `getAdler()` and a
standalone `Adler32` over the same bytes can now never disagree.

Boundary contract, which is where CRC32 and Adler32 differ most:

* **No complement dance.** `CRC32`'s native operates on the public value while
  zlib's inner loop runs on `~crc`; `crc32_update_public` hides that. Adler-32
  has no such split — the RFC 1950 state *is* the value `getValue()` returns
  (`(long) adler & 0xffffffffL`).
* **A fresh checksum is `1`, not `0`** (`s1 = 1`, `s2 = 0`). The seed arrives
  from the Java field initialiser; the natives never invent it.
* `update(int adler, int b)` masks `b` to its low 8 bits (jbyte truncation).
* `updateByteBuffer` reads `addr + off` for `len` bytes via
  `ctx.copy_from_native_memory`, exactly like `crc32_update_byte_buffer_0`, and
  raises `IOException` with `"Adler32.updateByteBuffer: failed to read native
  buffer memory"` if the read fails rather than checksumming a zero buffer.
* A null array is a no-op returning the input `adler` unchanged, matching the
  byte[] path's `read_byte_array` behaviour elsewhere in the file.

## Hand-verified test vectors

`adler32_known_vectors` and `adler32_natives_match_vectors` in the file's
`#[cfg(test)]` module pin these. Each was computed by hand from
`a = (a + byte) % 65521`, `b = (b + a) % 65521`, result `(b << 16) | a`:

| input | s1 (`a`) | s2 (`b`) | result |
| --- | --- | --- | --- |
| `""` | 1 | 0 | `0x00000001` |
| `"a"` | `1+97 = 98 = 0x62` | `0+98 = 98 = 0x62` | `0x00620062` |
| `"abc"` | `1+97+98+99 = 295 = 0x127` | `98+196+295 = 589 = 0x24D` | `0x024D0127` |
| `"123456789"` | `1 + Σ(49..57) = 1+477 = 478 = 0x1DE` | `50+100+151+203+256+310+365+421+478 = 2334 = 0x91E` | `0x091E01DE` |

`0x024D0127` is the standard zlib/NIST Adler-32 check value for `"abc"`.
`0x091E01DE` is what `RJdkJni.java:119` asserts and what HotSpot 25 prints
(`adler=152961502`).

The `"123456789"` case is additionally checked **split** as `"1234"` then
`"56789"`, because the concatenation property is exactly what
`Adler32.update` relies on across successive native calls, and it is the
property a naive `s1`/`s2` reduction placed outside the chunk loop would break.
`updateBytes` is exercised over `"XX123456789XX"` with `off=2, len=9` so the
slice arithmetic is not vacuous.

## What else `RJdkJni.zipNatives` touches (checked, all already registered)

Fixing only the method named in the error would have moved the
`UnsatisfiedLinkError` to the next line, so the whole function was audited:

* `CRC32` KATs (lines 111-116) — already pass in the strict arm; covered by
  `update(II)I` / `updateBytes(I[BII)I` / `updateBytes0(I[BII)I`.
* `Deflater(BEST_COMPRESSION)` round-trip, `getBytesRead`/`getBytesWritten`,
  `end`, double-`end`, use-after-`end` (lines 121-179) — `init(IIZ)J`,
  `deflateBytesBytes`, `reset`, `end` all registered.
* `Inflater` round-trip and the corrupt-input `DataFormatException` (lines
  142-165) — `init(Z)J`, `inflateBytesBytes`, `end` all registered.
* `CRC32C` is **not** used by `zipNatives`; `zip_crc32c.rs` was left alone.

Nothing after `zipNatives` (`libraryLoading`, `unboundNative`,
`referenceIdentity`) is a `java.util.zip` surface.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins zip_real::tests::adler32

# both arms must reach PASS RJdkJni (35 checks), byte-identical to HotSpot:
javac -d regression-suite/build regression-suite/src/RJdkJni.java
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkJni
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkJni
java -cp regression-suite/build RJdkJni      # HotSpot 25 oracle
```

The line that closes this is `CK RJdkJni crc32=3421780262 adler=152961502
roundTrip=true`. `adler=152961502` is `0x091E01DE`; any other value means the
seed or the modulus is wrong, not the registration.

## Baselines: none need re-freezing

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — the gate scores only
  rows present in *both* baseline and census ("Adding a native is not this
  gate's business... New rows pass and are reported").
* `scripts/baselines/jdk-only-bridge-ratchet.json` — freezes
  `bridge_shadows_bytecode` and `bridge_without_acc_native`. All three new rows
  are `ACC_NATIVE`-backed and shadow no bytecode, so both frozen counts are
  unchanged.
* `native-builtins/tests/stub_ratchet.rs` — counts `SyntheticStub` only;
  unchanged at 689.
* `scripts/baselines/jdk-only-dead-everywhere.tsv` carries `file:line`
  provenance and its one `zip_real.rs` row (`Deflater.initIDs`, was line 1046)
  has drifted by the inserted section. No script in the tree reads that file —
  it is a documentation-grade snapshot, regenerated when someone re-measures —
  so it is deliberately left alone rather than hand-edited.
