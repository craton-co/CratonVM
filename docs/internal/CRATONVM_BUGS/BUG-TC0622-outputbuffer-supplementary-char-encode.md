# Bug TC0622 — supplementary (surrogate-pair) chars written one `char` at a time encode to U+FFFD instead of the astral code point (CratonVM's streaming encoder shims have no cross-call high-surrogate carry)

> **✅ FIXED on dev (2026-06-23, merge `0c776040` / fix `a7f65b51`).**
> Threaded a cross-call high-surrogate carry through both encoder shims, mirroring
> the existing decoder fix. Three coordinated parts:
> 1. **`native-api` engine** — `encode_utf8_strict` now reports a *trailing* lone
>    high surrogate as `Incomplete` (not `Malformed`); the lossy path is unchanged.
> 2. **`native-io` StreamEncoder shim** (OutputStreamWriter path) — carries the
>    unmatched high surrogate across `write()` calls via the REAL JDK
>    `haveLeftoverChar`/`leftoverChar` primitive fields (indexed scratch slots alias
>    the real reference-typed fields and can't round-trip a primitive); leftover
>    flushed as U+FFFD at close.
> 3. **`native-builtins` `CharsetEncoder.encode`** (Coyote `C2BConverter` path) —
>    rewritten to encode atom-by-atom (BMP unit / full surrogate pair) so it leaves
>    a trailing high surrogate buffered (UNDERFLOW), stops OVERFLOW on a whole-atom
>    boundary (no truncated multi-byte sequence, exact input consumption — the prior
>    proportional estimate corrupted the body when bb filled mid-character), and
>    honours the encoder's REPLACE actions (was throwing a spurious
>    `MalformedInputException` at a chunk boundary).
>
> **Verified:** per-`char` `OutputStreamWriter(UTF-8)` of `U+10000..` byte-identical
> to HotSpot; `org.apache.catalina.connector.TestOutputBuffer` **PASS** (was FAIL).
> 5 new unit tests; no regression in `buf/*` converter + writer tests (failure
> counts identical to dev baseline).

> **One-line root cause:** CratonVM's streaming character-encoder shims
> (`sun.nio.cs.StreamEncoder` in `native-io/src/stream_encoder.rs` and the
> Coyote-path `CharsetEncoder.encode(CharBuffer,ByteBuffer,boolean)` in
> `native-builtins/src/charset.rs`) encode each input chunk **independently and
> immediately**, holding **no pending-high-surrogate state** between calls and
> **ignoring the `endOfInput` flag**. When a UTF-16 surrogate **pair** is split
> across two writes — exactly what `Writer.write(char)` per-character writing does
> (the Tomcat test, and `OutputStreamWriter(UTF-8)`) — each lone surrogate is
> encoded on its own. The strict UTF-8 encoder reports it malformed; under the
> `REPLACE` action the encoder substitutes **U+FFFD (`EF BF BD`)** for *each*
> surrogate. So one astral code point `U+10000` (`F0 90 80 80`) becomes two
> 3-byte replacement chars (`EF BF BD EF BF BD`), corrupting the response body.

**Severity:** Medium-High (silent data corruption of any supplementary-plane /
emoji / CJK-Ext text whenever it is written a `char` at a time through a servlet
`Writer` or any `OutputStreamWriter`/`StreamEncoder`; the decoder path was
already fixed for the symmetric split, the encoder path was not).

**Status on CratonVM:** FAIL (body corrupted). **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`,
exe `cratonvm-tcfull-0622.exe`).

**Affected classes:**
`org.apache.catalina.connector.TestOutputBuffer` (method `testUtf8SurrogateBody`).
General: any code writing supplementary chars one `char` at a time through
`java.io.OutputStreamWriter`/`sun.nio.cs.StreamEncoder` or through Coyote's
`OutputBuffer`→`C2BConverter`→`CharsetEncoder` per-char path.

## Symptom

`testUtf8SurrogateBody` builds a String of one `'a'` followed by every
supplementary code point in `U+10000..U+10FFF` (via `Character.toChars(i)`),
serves it through a servlet that writes the body **one `char` at a time**
(`for (char aChar : chars) w.write(aChar);`), reads it back as UTF-8, and asserts
equality:

```
1) testUtf8SurrogateBody(org.apache.catalina.connector.TestOutputBuffer)
java.lang.AssertionError: expected:<a𐀀𐀁...(long run of U+10000+ chars)...> but was:<null>
Tests run: 3,  Failures: 1
```

(The `but was:<null>` is a downstream effect — the corrupted/over-long body and
the malformed-input report perturb Coyote's content-length / read-back so
`ByteChunk.toString()` yields no usable string. The underlying defect is the
encode corruption demonstrated directly below.)

## Root cause (the encode path; native pinned)

The test exercises `Writer.write(char)` per character, so a single astral code
point arrives as a **high surrogate in one write and a low surrogate in the
next**. Two distinct CratonVM encoder shims handle this — both have the same
gap:

1. **`native-io/src/stream_encoder.rs` (`sun.nio.cs.StreamEncoder`)** — the path
   behind `OutputStreamWriter`. Its state is only 3 fields (output / name /
   closed); there is **no surrogate-carry slot**. `native_se_write_int`
   (`write(int c)`) calls `write_bytes(ctx, this, &[c])` with a **single** code
   unit, and `write_bytes` immediately runs
   `engine::encode_chars_lossy(name, &[c])` (stream_encoder.rs:198). A lone high
   surrogate is encoded right then — there is no way to wait for the matching low
   surrogate in the following call.

2. **`native-builtins/src/charset.rs` (`native_encoder_encode`,
   `CharsetEncoder.encode(CharBuffer,ByteBuffer,boolean)`)** — the Coyote
   `C2BConverter` path. It reads the input chunk and calls
   `engine::encode_chars(name, &chars)` (charset.rs:421). On a high surrogate at
   the **end of the chunk** the engine returns `CodingErrorKind::Malformed`, and
   the native maps everything-not-Unmappable to `CR_MALFORMED` (charset.rs:426).
   It **never reads `args[3]` (endOfInput)** and has **no
   `Incomplete && !end_of_input` branch** to leave the trailing high surrogate
   buffered for the next call — even though the *symmetric* decoder
   (`native_decoder_decode`, charset.rs:512) was already fixed to do exactly that.

The engine itself (`native-api/src/charset.rs`) is correct *per chunk*:
`encode_utf8_strict` (line 292) correctly encodes a **paired** surrogate and
correctly REPORTs a lone one as `Malformed`; the `*_lossy` wrapper substitutes
U+FFFD (`encode_utf8` → `String::from_utf16_lossy`, line 277). The bug is purely
the **streaming state**: neither shim carries an unmatched high surrogate to the
next call, so a pair split across two writes is seen as two lone surrogates and
each becomes U+FFFD.

### Confirmed with a minimal repro (no Tomcat needed)

`scratch .tooling/drv/SurrEnc.java`: build the test data, write it (a) whole via
`String.getBytes(UTF_8)` and (b) one `char` at a time through
`OutputStreamWriter(UTF_8)`:

| path | HotSpot | CratonVM |
|------|---------|----------|
| whole-string `getBytes(UTF-8)` (pairs intact) | 65 bytes | 65 bytes ✅ |
| per-`char` `OutputStreamWriter(UTF-8)` (pairs split) | **65 bytes** | **97 bytes** ❌ |
| round-trip per-char output `equals` original | `true` | `false` |

First bytes of the per-char CratonVM output:
`97 239 191 189 239 191 189 …` = `a` then `EF BF BD` (U+FFFD) for the high
surrogate, `EF BF BD` for the low surrogate — i.e. each half of every pair
became a replacement char. HotSpot emits `97 240 144 128 128 …` =
`a F0 90 80 80` (correct `U+10000`). The whole-string path is byte-identical on
both VMs, isolating the defect to the **streaming/split** case.

## Reproduction

Full test:

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
& C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.catalina.connector.TestOutputBuffer
# => 1 failure: testUtf8SurrogateBody
```

Minimal standalone repro (`.tooling/drv/SurrEnc.java`, no server):

```java
char[] chars = "a𐀀".toCharArray();            // 'a' + U+10000
ByteArrayOutputStream baos = new ByteArrayOutputStream();
Writer w = new OutputStreamWriter(baos, StandardCharsets.UTF_8);
for (char c : chars) w.write(c);                          // splits the pair
w.flush();
// HotSpot: bytes = 97, F0 90 80 80  (5 bytes)
// CratonVM: bytes = 97, EF BF BD, EF BF BD (7 bytes) — each surrogate -> U+FFFD
```

## Recommendation

**FIX.** Give both streaming encoder shims a **pending-high-surrogate carry**,
mirroring the decoder fix that already exists (`native_decoder_decode`'s
`Incomplete && !end_of_input` branch, charset.rs:512) and the
`pending_low_surrogate` carry already used by the reader side
(`native-io/src/lib.rs`):

1. **`native-builtins/src/charset.rs` `native_encoder_encode`:** read
   `args[3]` (endOfInput); make `encode_utf8_strict` distinguish a **trailing
   lone high surrogate** as `CodingErrorKind::Incomplete` (not `Malformed`);
   when `!end_of_input` and the error is `Incomplete`, encode only the prefix,
   advance the CharBuffer position to leave the lone high surrogate buffered, and
   return `CR_UNDERFLOW` (symmetric to the decoder). Only at end-of-input is a
   still-unpaired surrogate genuinely malformed.

2. **`native-io/src/stream_encoder.rs`:** add a 4th field (or a side-table
   keyed on `this`) holding an optional pending high surrogate. In `write_bytes`,
   prepend any carried high surrogate, and if the chunk ends on a lone high
   surrogate, stash it instead of encoding it; emit it (as U+FFFD or per the
   configured action) at `flush`/`close`. Equivalent to the JDK's
   `StreamEncoder.implWrite` keeping the encoder's internal leftover state.

Bounded once the carry state is threaded through; the per-chunk engine is
already correct, so no encoder-table work is needed. High value: silently
corrupts all supplementary-plane text written incrementally through a `Writer`.
```
