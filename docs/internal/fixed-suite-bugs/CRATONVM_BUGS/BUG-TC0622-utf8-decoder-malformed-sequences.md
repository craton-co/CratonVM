# BUG-TC0622 — UTF-8 `CharsetDecoder.decode` ignores `CodingErrorAction.REPLACE` (returns MALFORMED instead of substituting U+FFFD)

> **✅ FIXED 2026-06-23** — merged to `dev` `a28c1975` (branch
> `fix/tc0622-utf8-conformance`), completing the partial fix `0a7298d1`. Both
> remaining follow-ups are closed:
>
> 1. **Full `TestUtf8` conformance (decoder).** `decode_utf8_lossy`
>    (`String::from_utf8_lossy`) diverged from `sun.nio.cs.UTF_8.Decoder` at
>    multi-byte-sequence *buffer boundaries* (the test feeds bytes one at a time).
>    Ported the JDK `decodeArrayLoop` + `malformedN` length accounting into
>    `cratonvm_native_api::charset::utf8_decode` — a `CharsetDecoder.decode`
>    orchestrator honoring REPORT/REPLACE/IGNORE + `endOfInput` — and routed the
>    native UTF-8 decode path through it. All **53** `TestUtf8` cases now match
>    byte-for-byte (engine unit test replays all 53). Non-UTF-8 charsets keep the
>    existing path.
> 2. **`TestUEncoder` (encoder).** `native_encoder_encode` now runs the
>    `CharsetEncoder.encode` orchestrator: encodes atom-by-atom (a surrogate pair
>    is one atom), honors `malformedInputAction`/`unmappableCharacterAction=REPLACE`,
>    and — crucially — defers a lone high surrogate at the end of the input buffer
>    as UNDERFLOW when `!endOfInput` (a potential pair start) rather than raising
>    `MalformedInputException`. This lets `C2BConverter` carry a surrogate across
>    its `leftovers` buffer.
>
> **Validation:** `TestUtf8` OK(1), `TestUEncoder` OK(1), `TestB2CConverter`
> stays PASS OK(7); `native-api` + `native-builtins` charset unit tests green.

> **🟡 PARTIALLY FIXED 2026-06-23** — merged to `dev` `0a7298d1` (branch
> `fix/tomcat-batch2`). The native `CharsetDecoder.decode` now reads
> `malformedInputAction` and, in REPLACE mode, substitutes U+FFFD via
> `engine::decode_bytes_lossy` instead of returning MALFORMED (default REPORT
> path byte-identical → no regression; `TestB2CConverter` stays PASS). This
> **advances `TestUtf8` from case 5 → case 14** (the documented `A + 4×U+FFFD + A`
> case now matches) but does **not** fully pass the strict 53-case suite. **Two
> deeper follow-ups remain:** (1) `decode_utf8_lossy` uses Rust's
> `String::from_utf8_lossy`, whose maximal-subpart replacement granularity
> diverges from the JDK's at case 14+ — full conformance needs a JDK-exact UTF-8
> malformed-length state machine; (2) **`TestUEncoder` is the ENCODER side** —
> `native_encoder_encode` needs the analogous `unmappableCharacterAction=REPLACE`
> handling (still FAIL). So the REPLACE-action *contract* is now honored on the
> decoder, but byte-for-byte JDK conformance + the encoder path are separate work.

> **Root cause (one line):** CratonVM's native `CharsetDecoder.decode(ByteBuffer,
> CharBuffer, boolean)` (`native-builtins/src/charset.rs::native_decoder_decode`)
> unconditionally returns `CR_MALFORMED` on a malformed/out-of-range byte
> sequence and **never reads the decoder's configured `malformedInputAction`** —
> so a decoder set to `CodingErrorAction.REPLACE` reports an error to the caller
> instead of substituting U+FFFD and continuing, the way the real
> `java.nio.charset.CharsetDecoder.decode()` orchestrator does.

**Severity:** Medium (breaks any code that decodes untrusted/malformed bytes with
the REPLACE action — the standard "lenient" decode path; also surfaces as a
spurious `MalformedInputException` in URL/percent-encoding code).
**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-23
**Binary:** dev `df11ac00` (exe `C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe`).
**Affected classes (2):**
`org.apache.tomcat.util.buf.TestUtf8` (`testJvmDecoder`),
`org.apache.tomcat.util.buf.TestUEncoder` (`testEncodeURLWithSlashInit`).

This is the **DECODE-side sibling** of the already-documented surrogate ENCODE
bug ([`BUG-TC0622-outputbuffer-supplementary-char-encode.md`](BUG-TC0622-outputbuffer-supplementary-char-encode.md))
and a follow-on to [`BUG-K-charsetdecoder-endofinput.md`](BUG-K-charsetdecoder-endofinput.md):
BUG-K taught the native body to honour `endOfInput`; it still ignores the
configured **error action**.

## Symptom

### TestUtf8.testJvmDecoder — the exact diverging case

`TestUtf8` feeds malformed/boundary UTF-8 byte sequences one byte at a time and
checks decoder behaviour first with `CodingErrorAction.REPORT`, then with
`CodingErrorAction.REPLACE` (`TestUtf8.doTest`, lines 338–391). The error log:

```
Executed 5 of 53 UTF-8 tests before encountering a failure
java.lang.AssertionError: Invalid code point - out of range
    at org.junit.Assert.fail(Assert.java:89)
    at org.apache.tomcat.util.buf.TestUtf8.doTest(TestUtf8.java:376)
    at org.apache.tomcat.util.buf.TestUtf8.testJvmDecoder(TestUtf8.java:325)
```

Test cases 0–4 are all **valid** sequences (zero-length, valid 1/2/3/4-byte) and
pass. **Test case index 5 — the first malformed case — diverges.** Its
description string is literally `"Invalid code point - out of range"`
(`TestUtf8.java:72-76`):

```java
new Utf8TestCase(
    "Invalid code point - out of range",
    new int[] {0x41, 0xF4, 0x90, 0x80, 0x80, 0x41},   // bytes
    2,                                                 // expected REPORT error at byte index 2
    "A����A");                      // expected REPLACE output (4× U+FFFD)
```

The bytes `F4 90 80 80` decode to U+110000, which is **out of range** (the maximum
Unicode code point is U+10FFFF; the largest 4-byte lead+continuation that stays in
range is `F4 8F BF BF` = U+10FFFF, so `90` > `8F` is malformed at the 2nd byte).

- **REPORT phase passes:** the decoder correctly reports an error, and the test
  asserts the error occurs at byte index 2 (`assertEquals(expected=2, i)`) — this
  part agrees with HotSpot.
- **REPLACE phase FAILS (line 376):** after `decoder.onMalformedInput(REPLACE)`
  the test feeds the same bytes and expects **no** `cr.isError()` (the decoder is
  supposed to substitute U+FFFD). CratonVM still returns `CR_MALFORMED`, so
  `cr.isError()` is true → `Assert.fail(testCase.description)` fires with the
  case description as the message. Hence the message *"Invalid code point - out of
  range"* is the test **case name**, not a VM-emitted string; the real defect is
  "REPLACE not honoured at line 376".

HotSpot in REPLACE mode emits `A����A` (one U+FFFD per
malformed byte) and never reports an error — so the test passes there.

### TestUEncoder.testEncodeURLWithSlashInit

Same root cause on the percent-encoder path. `UEncoder.encodeURL` →
`C2BConverter.convert` decodes via the configured decoder and the malformed result
is thrown straight through:

```
java.nio.charset.MalformedInputException
    at java.nio.charset.CoderResult.throwException(CoderResult.java:279)
    at org.apache.tomcat.util.buf.C2BConverter.convert(C2BConverter.java:139)
    at org.apache.tomcat.util.buf.UEncoder.encodeURL(UEncoder.java:103)
    at org.apache.tomcat.util.buf.TestUEncoder.testEncodeURLWithSlashInit(TestUEncoder.java:42)
```

A `CoderResult` carrying MALFORMED leaks out where the configured action should
have absorbed it (REPLACE) or where a correct REPORT path would not have flagged a
sequence HotSpot decodes cleanly.

## Root cause (pinned to the decoder)

`native-builtins/src/charset.rs::native_decoder_decode` (≈ lines 470–549) is the
native body bound to `java.nio.charset.CharsetDecoder.decode(ByteBuffer,
CharBuffer, boolean)`. Its error handling (lines 510–530) is:

```rust
let (decoded, input_len) = match engine::decode_bytes(&name, &bytes) {
    Ok(chars) => (chars, bytes.len()),
    Err(e) if e.kind == engine::CodingErrorKind::Incomplete && !end_of_input => {
        // ... buffer the partial sequence, return UNDERFLOW (BUG-K fix) ...
    }
    Err(e) => {
        set_pos(ctx, bb, bpos + e.offset as i32);
        let r = alloc_coder_result(ctx, CR_MALFORMED);   // <-- ALWAYS, regardless of action
        return Ok(Some(Value::Object(Some(r))));
    }
};
```

The malformed branch is **unconditional**: it returns `CR_MALFORMED` no matter
what `onMalformedInput(...)` was set to. A grep of the file confirms it never
reads `malformedInputAction` / `unmappableCharacterAction` / `replaceWith`
(`CodingErrorAction` appears only in a doc comment at line 301).

In the real JDK, `CharsetDecoder.decode(in, out, endOfInput)` is a **final**
orchestrator that calls the per-charset `decodeLoop` and then *applies the
configured action* to a MALFORMED/UNMAPPABLE result:
- **REPORT** → return the error result to the caller (CratonVM's current behaviour
  — correct for the REPORT phase),
- **REPLACE** → write `replaceWith()` (default `"�"`) to the output, advance
  the input past the malformed bytes, and continue the loop — **never** surfacing
  an error,
- **IGNORE** → skip the bytes silently.

CratonVM collapses the whole orchestrator into the native body and only ever
implements REPORT. Because the engine's `decode_bytes` already returns a precise
`CodingError { offset, length, kind }` (see `native-api/src/charset.rs::decode_utf8`,
lines 249–268, which sets `length` to the malformed run via Rust's
`error_len()`), the action-aware REPLACE path is implementable here: on
`Malformed`/`Unmappable` when the decoder's action is REPLACE, emit the decoder's
`replaceWith` units (default U+FFFD), advance the input position by `length`
bytes, and resume decoding the remainder (one U+FFFD **per malformed byte**, to
match HotSpot's `malformedInputLength` accounting — the expected output has 4
U+FFFD for the 4 bad bytes `F4 90 80 80`).

Note this is a **decoder-orchestrator** gap, not an engine bug: `decode_bytes`
correctly classifies `F4 90 80 80` as Malformed at offset 1 / length 4 (the `A`
prefix already drained). The lossy engine variant `decode_bytes_lossy`
(`native-api/src/charset.rs:152`) already produces the U+FFFD substitution; the
native decode body simply doesn't route REPLACE-action decoders through it.

## Reproduction

`TestUtf8` / `TestUEncoder` are pure unit tests — no server, no special props.

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.tomcat.util.buf.TestUtf8
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.tomcat.util.buf.TestUEncoder
```

Minimal standalone repro (no Tomcat):

```java
byte[] bad = { 0x41, (byte)0xF4, (byte)0x90, (byte)0x80, (byte)0x80, 0x41 }; // U+110000, out of range
CharsetDecoder d = StandardCharsets.UTF_8.newDecoder()
    .onMalformedInput(CodingErrorAction.REPLACE)
    .onUnmappableCharacter(CodingErrorAction.REPLACE);
String s = d.decode(ByteBuffer.wrap(bad)).toString();
// HotSpot: "A����A"  | CratonVM: MalformedInputException (BUG)
```

## Recommendation

**FIX — charset decoder, bounded.** Make `native_decoder_decode` action-aware:
read the decoder object's `malformedInputAction` / `unmappableCharacterAction`
fields (the `CodingErrorAction` set by `onMalformedInput`/`onUnmappableCharacter`)
and its `replaceWith` string. On a `Malformed`/`Unmappable` engine error:
- **REPORT** → current behaviour (return `CR_MALFORMED`/`CR_UNMAPPABLE`),
- **REPLACE** → emit the `replaceWith` units (one U+FFFD per malformed byte to
  match HotSpot's per-byte substitution), advance the input by the error
  `length`, and continue decoding the remaining bytes in the buffer,
- **IGNORE** → advance past the malformed run without emitting.

The engine already supplies offset+length and a lossy variant
(`decode_bytes_lossy`), so this is a localized change in `native-builtins/src/charset.rs`
(plus possibly threading `replaceWith`/action accessors through). It will fix
both affected classes at once. Validate against `TestUtf8` (all 53 cases) and
`TestUEncoder`, and cross-check that the REPORT phase still passes (it must keep
returning the error at the correct byte index).

**Relation to the encode-side bug:** the encode path was hardened analogously —
`encode_utf8_strict` (`native-api/src/charset.rs:292`) REPORTs lone surrogates on
the strict path while the lossy callers substitute U+FFFD. The decode path needs
the same action-driven split surfaced through `native_decoder_decode`. Together
with `BUG-TC0622-outputbuffer-supplementary-char-encode.md` (encode) and `BUG-K`
(endOfInput), this completes the `CharsetDecoder`/`CharsetEncoder` error-action
contract.
