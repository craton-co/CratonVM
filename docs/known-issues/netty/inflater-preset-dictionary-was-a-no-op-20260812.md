# `Inflater`/`Deflater.setDictionary` were no-ops, so every SPDY header block failed

**Status:** FIXED (2026-08-12) in `native-builtins/src/zip_real.rs`. Found while
working [investigate-batch-09.md](investigate-batch-09.md).

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

## Fix

Both natives now call flate2's `set_dictionary` on the live
`Decompress`/`Compress`. Errors are swallowed rather than thrown: the JDK's own
native returns `void`, and its only documented failure is a dictionary whose
checksum does not match the stream's `DICTID` — which leaves
`needsDictionary()` true, so the caller's next `inflate()` makes no progress
and the stream fails where it would have failed anyway.

The direct-`ByteBuffer` overloads are left as no-ops deliberately: the
direct-buffer *inflate* natives are themselves unsupported
(`infl_direct_buffer_unsupported`), so a caller cannot reach a state where the
buffer-flavoured `setDictionary` matters, and half-wiring one member of that
family would be worse than the honest gap.

## Result

```
SpdyHeaderBlockZlibDecoderTest   before: found=9 ok=5 failed=4
                                  after: found=9 ok=9 failed=0   (matches HotSpot)
SpdyFrameDecoderTest                     found=60 ok=60          (unchanged)
SpdyUnknownFrameDecoderTest              found=1  ok=1           (unchanged)
```

The blast radius is wider than SPDY: any protocol or file format that uses a
zlib preset dictionary was affected, in both directions — CratonVM could
neither read such a stream nor produce one a real zlib peer would accept.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
<cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m @common.args -Dcraton.batch=1 \
    CratonRunner io.netty.handler.codec.spdy.SpdyHeaderBlockZlibDecoderTest
```

A 40-line standalone probe (`Deflater.setDictionary` → compare compressed
length, then `Inflater.needsDictionary`/`setDictionary`) reproduces it with no
netty on the classpath; compare the numbers in the table above against
`java`.
