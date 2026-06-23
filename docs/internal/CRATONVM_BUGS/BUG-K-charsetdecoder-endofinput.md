# BUG-K — interpreter mis-decodes `CharsetDecoder.decode(in, out, endOfInput=false)` as malformed

> **STATUS: FIXED.** Was a latent pre-existing bug (reproduced on plain `dev`
> with `CRATONVM_DISABLE_JIT=1`; JIT normally masked it). Surfaced while
> evaluating [BUG-H](BUG-H-jit-exception-table-escape.md)
> (`TestB2CConverter.testBug54602c`).
>
> **FIX (`native-api/src/charset.rs` + `native-builtins/src/charset.rs`):** The
> native `CharsetDecoder.decode(ByteBuffer, CharBuffer, boolean)` ignored the
> `endOfInput` argument and reported every decode failure as MALFORMED. Added a
> `CodingErrorKind::Incomplete` (set by `decode_utf8` when
> `std::str::from_utf8`'s `error_len() == None` — a truncated trailing
> sequence); when `endOfInput == false`, `native_decoder_decode` now decodes
> only the valid prefix, leaves the partial bytes buffered, and returns
> UNDERFLOW. Verified: the repro returns UNDERFLOW for `endOfInput=false`,
> `TestB2CConverter` 7/7.

## Symptom

A partial multi-byte UTF-8 sequence decoded with `endOfInput=false` must return
`UNDERFLOW` (wait for more bytes); only `endOfInput=true` should report
`MALFORMED`. CratonVM's interpreter reports `MALFORMED` for **both**:

```java
byte[] partial = { (byte)0xE2 };                 // first byte of a 3-byte seq
CharsetDecoder d = StandardCharsets.UTF_8.newDecoder()
    .onMalformedInput(CodingErrorAction.REPORT);
ByteBuffer in = ByteBuffer.wrap(partial);
CharBuffer out = CharBuffer.allocate(10);
d.decode(in, out, false);  // HotSpot: UNDERFLOW   | CratonVM: MALFORMED  (BUG)
d.decode(in, out, true);   // HotSpot: MALFORMED   | CratonVM: MALFORMED
```

```
cratonvm (no-JIT):  endOfInput=false: underflow=false malformed=true   <-- wrong
HotSpot:            endOfInput=false: underflow=true  malformed=false
```

## Notes / next steps

- Not a generic boolean-3rd-parameter bug — a minimal `m(Object,Object,boolean)`
  with a ternary + try/catch reads the boolean correctly in the interpreter.
- So the defect is inside the actual decode machinery — `CharsetDecoder.decode`'s
  `if (cr.isUnderflow()) { if (endOfInput && in.hasRemaining()) … }` state loop or
  the `sun.nio.cs.UTF_8$Decoder` `decodeLoop`/`malformedForLength` path —
  effectively behaving as if `endOfInput` were always true (or `decodeLoop`
  itself returning malformed for a trailing partial sequence).
- Repro: `apps/tomcat/.tooling/Dec.java`.
