# `Inflater`/`Deflater.setDictionary` were no-ops, so every SPDY header block failed

**Status:** ✅ FIXED (2026-08-12) in `native-builtins/src/zip_real.rs`, both the
`byte[]` overloads and — as of the retirement pass the same day — the direct
`ByteBuffer` overloads that had been left behind. Found while working
[investigate-batch-09.md](investigate-batch-09.md).

## Symptom

`io.netty.handler.codec.spdy.SpdyHeaderBlockZlibDecoderTest` — 9/9 on stock
HotSpot JDK 25, 5 ok / 4 failed on CratonVM. Every failure was the same:

```
io.netty.handler.codec.spdy.SpdyProtocolException: Invalid Header Block
```

## Root cause

SPDY compresses header blocks with zlib using a **preset dictionary**. The
stream advertises it by setting `FDICT` in the header's second byte and
following the header with the dictionary's ADLER-32; zlib then reports
`Z_NEED_DICT`, the JDK surfaces that as `Inflater.needsDictionary()`, and the
caller supplies the dictionary and resumes. `SpdyHeaderBlockZlibDecoder` does
exactly that:

```java
int numBytes = decompressor.inflate(out, off, decompressed.writableBytes());
if (numBytes == 0 && decompressor.needsDictionary()) {
    decompressor.setDictionary(SPDY_DICT);
    numBytes = decompressor.inflate(out, off, decompressed.writableBytes());
}
```

`Inflater.setDictionary` was a **deliberate no-op** in CratonVM:

```rust
fn infl_set_dictionary(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // flate2's `Decompress::set_dictionary` is gated behind a zlib backend
    // feature we don't enable; preset dictionaries are not used by any
    // JAR/ZIP entry encountered on the bootstrap path, so we no-op here.
    Ok(None)
}
```

**The stated reason was not true.** `native-builtins/Cargo.toml` builds flate2
as `flate2 = { version = "1", features = ["zlib"] }`, which sets flate2's
`any_zlib` and compiles `set_dictionary` in. The capability had been available
the whole time; only the binding was missing. `Deflater.setDictionary` was a
no-op for the same stated reason.

So the decoder handed over the dictionary, got silence, inflated 0 bytes, and
netty reported `Invalid Header Block`.

## Why a round-trip test would not have caught it

Both halves ignored the dictionary, so CratonVM compressing and then
decompressing its own output worked perfectly — self-consistent and wrong. The
divergence is only visible against a real zlib peer, and the cheapest way to
see it is the **compressed length**, not the round trip:

| | HotSpot JDK 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `Deflater.setDictionary` then `deflate` | 57 bytes | **75 bytes** | 57 bytes |
| first `inflate` return / `needsDictionary()` | 0 / **true** | 78 / **false** | 0 / **true** |
| `setDictionary` + second `inflate` | 78 bytes | (never reached) | 78 bytes |
| round trip against itself | OK | OK | OK |

75 bytes is exactly what plain deflate produces — the dictionary was never
applied. The last row is the trap: it is green in all three columns.

The failing netty test does not rely on CratonVM's encoder at all. It
hand-builds the zlib header as `{0x78, 0x3f, 0xe3, 0xc6, 0xa7, 0xc2}` — `0x3f`
has bit 5 (`FDICT`) set, and the four bytes after it are the SPDY dictionary's
`DICTID` — so the decoder is fed a stream that *demands* a preset dictionary
from the first byte.

## Fix, part 1 — the `byte[]` overloads

Both natives now call flate2's `set_dictionary` on the live
`Decompress`/`Compress`. Errors are swallowed rather than thrown: the JDK's own
native returns `void`, and its only documented failure is a dictionary whose
checksum does not match the stream's `DICTID` — which leaves
`needsDictionary()` true, so the caller's next `inflate()` makes no progress
and the stream fails where it would have failed anyway.

## Fix, part 2 — the direct-`ByteBuffer` overloads (the residual)

The first fix left `setDictionaryBuffer` — the native behind
`setDictionary(ByteBuffer)` for a **direct** buffer — a no-op on both sides,
with this reason:

```rust
fn infl_set_dictionary_buffer(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Direct-buffer overload: the direct-buffer inflate natives are themselves
    // unsupported (`infl_direct_buffer_unsupported`), so a caller cannot get
    // far enough to need this. Left a no-op deliberately rather than silently
    // half-wiring a path whose siblings throw.
    Ok(None)
}
```

**That reason was not true either, and it is the same failure of method as the
original.** There is no `infl_direct_buffer_unsupported` anywhere in the tree —
all four `inflate*` overloads and all four `deflate*` overloads read and write
direct-buffer memory for real (`copy_from_native_memory` /
`copy_to_native_memory`). The only surviving trace of the claim was a stale
`audit-round6` comment naming the function. So the path was reachable, and
`setDictionary(ByteBuffer)` reproduced the SPDY failure exactly, on a build
that had already "fixed" it:

| | HotSpot JDK 25 | CratonVM, dev tip `8763197f2` | CratonVM, after |
|---|---|---|---|
| `deflate`, no dictionary | 53 bytes | 53 | 53 |
| `deflate` after `setDictionary(byte[])` | 18 bytes | 18 | 18 |
| `deflate` after `setDictionary(ByteBuffer)` | **18 bytes** | **53 bytes** | **18 bytes** |
| zlib header after the `ByteBuffer` form | `78bb` (FDICT set) | **`789c`** (FDICT clear) | `78bb` |
| `inflate` via `setDictionary(ByteBuffer)` | 63 bytes, ok | **0 bytes, ok=false** | 63 bytes, ok |

Both natives now read the dictionary out of the buffer's native memory and hand
it to zlib. An address that cannot be read is reported as
`IllegalArgumentException`: that is UNCHECKED, and it is the exception
`setDictionary` documents. The sibling `inflate*` natives raise `IOException`
for the same fault, but they are declared `throws DataFormatException` and
`setDictionary*` is declared to throw nothing at all, so a checked throwable out
of this frame would escape every caller's `catch`.

**The lesson worth keeping:** a no-op whose comment explains why it is a no-op
is a claim, not a fact. Both halves of this bug were exactly that shape, and
both claims were checkable in under a minute — one against `Cargo.toml`, one
against `grep`.

## Result

```
SpdyHeaderBlockZlibDecoderTest   before: found=9 ok=5 failed=4
                                  after: found=9 ok=9 failed=0   (matches HotSpot)
SpdyFrameDecoderTest                     found=60 ok=60          (unchanged)
SpdyUnknownFrameDecoderTest              found=1  ok=1           (unchanged)
```

Re-measured on the direct-buffer fix (2026-08-12, `/data/cratonvm-nkir-fix`):

```
SpdyHeaderBlockZlibDecoderTest   found=9  ok=9  failed=0
SpdyFrameDecoderTest             found=60 ok=60 failed=0
SpdyUnknownFrameDecoderTest      found=1  ok=1  failed=0
JdkZlibTest                      found=24 ok=24 failed=0
ZlibCrossTest1                   found=10 ok=10 failed=0
ZlibCrossTest2                   found=10 ok=10 failed=0
```

The blast radius is wider than SPDY: any protocol or file format that uses a
zlib preset dictionary was affected, in both directions and through both the
heap-array and direct-buffer APIs — CratonVM could neither read such a stream
nor produce one a real zlib peer would accept.

## Regression cover

`native-builtins/src/zip_real.rs`:

* `set_dictionary_reaches_zlib_on_both_halves` — the `byte[]` pair, pinned on
  the emitted header's `FDICT` bit and on the `needDict` result bit;
* `set_dictionary_buffer_reaches_zlib_on_both_halves` — the direct-buffer pair,
  pinned on `FDICT` **and** on the compressed length against a no-dictionary
  control, because a round trip through our own encoder is green either way;
* `set_dictionary_buffer_reports_an_unreadable_address` — the unchecked-throw
  contract above, plus the zero-length case that must stay a no-op.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
<cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m @common.args -Dcraton.batch=1 \
    CratonRunner io.netty.handler.codec.spdy.SpdyHeaderBlockZlibDecoderTest
```

`ZipDictProbe.java` (~100 lines, no netty on the classpath) prints every row of
both tables above for whichever VM runs it — deflate with no dictionary, with a
`byte[]` dictionary and with a direct-`ByteBuffer` dictionary, then inflates
through both `setDictionary` flavours. Compare it against `java`.
