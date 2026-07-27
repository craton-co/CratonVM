# UTF-16 `CharsetEncoder.encode()` re-emits the byte-order-mark on every call instead of once per stream

**Status: OPEN — found 2026-07-23**

## Symptom

```
JUnit Jupiter:AppendableByteArrayTests:writesLargeStringWithExpandingBuffer()
    => org.opentest4j.AssertionFailedError:
expected: [-2, -1, 0, 84, 0, 104, 0, 105, 0, 115, ...]
 but was: [-2, -1, 0, 84, -2, -1, 0, 104, -2, -1, 0, 105, -2, -1, 0, 115, ...]
```

Expected UTF-16 output is `FE FF` (BOM, once) followed by 2-byte code units
for each character (`00 54` = 'T', `00 68` = 'h', ...). The actual output
repeats `FE FF` before **every single character**.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard8/logs/core_spring-boot.org.springframework.boot.json.AppendableByteArrayTests.out.log`

## Root cause

Confirmed at file:line, modulo the exact statefulness contract inside the
shared Rust `charset` engine (not traced further than the CratonVM/JVM
boundary).

`writesLargeStringWithExpandingBuffer()` constructs its `AppendableByteArray`
with a **4-byte** initial buffer and 4-byte expansion increment
(`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/json/AppendableByteArrayTests.java:71-73`),
so for UTF-16 (2 bytes/char) every single character overflows the buffer.
`AppendableByteArray.append(CharBuffer in)`
(`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/json/AppendableByteArray.java:89-103`)
handles overflow by growing the buffer and **recursively calling
`this.encoder.encode(in, this.out, false)` again on the same encoder
instance** — this is standard, correct usage of `CharsetEncoder`, and real
HotSpot's `sun.nio.cs.UTF_16$Encoder` tracks "have I already written the
BOM for this stream" as encoder-instance state that persists across
repeated `encode()` calls (only reset by `encoder.reset()`), so the BOM is
emitted exactly once regardless of how many `encode()` calls the stream
takes.

CratonVM's native override of `CharsetEncoder.encode`
(`native_encoder_encode`, `native-builtins/src/charset.rs:468` onward) calls
`engine::encode_chars(&name, &chars)` (line 505 fast path, and again per-atom
in the "precise path" starting at line 515) for **every** native `encode()`
invocation. This is a stateless, pure function of `(charset name, char
slice)` — it has no way to know whether this is a continuation of an
already-started "UTF-16" stream on the same `CharsetEncoder` object, so for
the BOM-emitting "UTF-16" charset it re-emits the BOM on every call. With a
buffer small enough to force one `encode()` call per character (as this
test deliberately does), that means one BOM per character instead of one
BOM total.

**Fix direction (not applied — investigation only):** thread a persistent
"have I already written the BOM for this encoder instance" flag through
`native_encoder_encode` (e.g. a field on the synthetic `CharsetEncoder`
carrier, mirroring `sun.nio.cs.UTF_16$Encoder.first`), and have
`engine::encode_chars` (or a `charset::encode_chars` variant it can call for
continuation calls) skip the BOM once that flag is set.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.json.AppendableByteArrayTests |
