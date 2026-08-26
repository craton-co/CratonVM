# `PrintStream.charset()` answers the ABSTRACT `java.nio.charset.Charset`, so anything that encodes through it dies

**Status: FIXED 2026-08-25.** Present in **both** modes, not just `--jdk-only`.

## 0. What it looks like

```text
java.lang.AbstractMethodError: method java/nio/charset/Charset.newEncoder()
                               Ljava/nio/charset/CharsetEncoder; has no Code attribute
  at sun/nio/cs/StreamEncoder.<init>(StreamEncoder.java:187)
  at sun/nio/cs/StreamEncoder.forOutputStreamWriter(StreamEncoder.java:70)
  at java/io/OutputStreamWriter.<init>(OutputStreamWriter.java:110)
  at java/util/logging/StreamHandler.setOutputStream(StreamHandler.java:130)
  at java/util/logging/ConsoleHandler.<init>(ConsoleHandler.java:81)
```

`new ConsoleHandler()` — one of the most ordinary things a Java program does.

## 1. The measurement, and the two claims it retracts

`probes/PrintStreamCharset.java`:

```text
                                        HotSpot   compatible   --jdk-only
System.out.charset()                    sun.nio.cs.MS1251   java.nio.charset.Charset (both)
System.out.charset().newEncoder()       ok        AbstractMethodError   AbstractMethodError
System.err.charset()                    sun.nio.cs.MS1251   java.nio.charset.Charset (both)
new PrintStream(baos).charset()         sun.nio.cs.UTF_8    java.nio.charset.Charset (both)
new PrintStream(baos,true,"UTF-8")      sun.nio.cs.UTF_8    sun.nio.cs.UTF_8      ok
new OutputStreamWriter(System.err)      ok        ok          AbstractMethodError

HotSpot PASS 9/9 · compatible FAIL 6 of 9 · --jdk-only FAIL 7 of 9
```

This record exists partly to retract two things the first draft of
`bug-a-refused-syntheticstub-…-20260824.md` §4 asserted about the same stack:

* **"a `--jdk-only` blocker"** — no. `PrintStream.charset()` is wrong in BOTH
  modes. Compatible mode only escapes the *crash* on the `OutputStreamWriter`
  path because a native covers it there; the charset object is equally broken.
* **"blocks the whole `StreamHandler` family"** — no. `new StreamHandler(baos,
  fmt)` is fine, and so is every `Charset.forName(...).newEncoder()`. Only a
  **`PrintStream` source** triggers it.

Both retractions came from one probe that split "the charset object" from "the
call that consumes it". The narrowing sequence is worth keeping: 14 independent
charset checks all passed, `StreamHandler(baos, fmt)` passed,
`StreamHandler(System.err, fmt)` failed — which put the blame on the STREAM, not
on charsets or on JUL.

## 2. Root cause

JDK 19+ `OutputStreamWriter(OutputStream out)` asks a `PrintStream` for its own
charset rather than assuming the default, then calls `newEncoder()` on the
result. CratonVM overrides `PrintStream.charset()` so it can never return null
— a real fix for a real NPE — but it built the replacement with
`charset_alloc`, which does:

```rust
try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 3)
```

**`java.nio.charset.Charset` is ABSTRACT**, and `newEncoder()` is one of its
abstract methods. So the override traded `NullPointerException("charset")` for
`AbstractMethodError` one call later — a worse error, further from the cause,
and in a JDK frame the caller did not write.

This is the same trap `panama::CRATON_SEGMENT_CLASS` and
`CRATON_BUFFER_POOL_CLASS` are named for: *an instance whose class is an
interface or an abstract base finds only abstract methods.* Both of those got a
concrete stand-in class. `charset_alloc` never did.

**The tell was in the measurement all along**: `new PrintStream(baos, true,
"UTF-8")` answered a real `sun.nio.cs.UTF_8`. A good answer was available; the
fabrication was reaching past it.

## 3. The fix — TWO sites, because one was not enough

`charset_concrete_or_synthetic(ctx, name)` asks the real `Charset.forName(name)`
first, accepts the result only when it is **not** the bare abstract base, and
falls back to `charset_alloc` otherwise.

The "not the bare base" check is not belt-and-braces: `Charset.forName` is
*itself* shimmed by `native_charset_for_name`, which calls `charset_alloc` — so
without the check the helper could ask, be handed the same abstract carrier
back, and report success. The fallback is kept for a synthetic-JDK image, where
there is no real class library to ask and an abstract carrier still beats a null.

**Pointing `PrintStream.charset()`'s fallback at that helper fixed only half of
it, and the half-fix is worth recording** because the measurement said so
immediately: 6-of-9 wrong became 4-of-9, and `new PrintStream(baos)` started
passing while `System.out` and `System.err` did not.

The reason is that `System.out`/`err` never reach the fallback. `install_charset`
(`lang_system.rs`) stamps their `charset` field at bootstrap, so the accessor's
"honour an already-set charset" branch returns the stamp and never synthesises
anything — and the stamp was `ctx.alloc_object(cs_class, …)` on the same
abstract `java/nio/charset/Charset`.

So both sites changed:

* **`install_charset`** now tries the real `Charset.forName` and keeps the
  hand-allocated stub only as a fallback. It runs during `initPhase1`, which is
  why the stub was reached for originally, so the real call is attempted
  defensively — bootstrap ordering decides which lands and neither outcome is
  worse than before.
* **`PrintStream.charset()`** now VALIDATES an already-set charset instead of
  trusting it: if the field holds the abstract base it resolves a concrete one,
  **writes the repair back**, and returns that. This runs lazily, long after
  bootstrap, so asking `Charset.forName` is safe here even when it was not
  there. The write-back matters because real `PrintStream` bytecode reads
  `charset` with a direct `getfield`, never through this accessor.

## 4. After

```text
                       compatible   --jdk-only
PrintStreamCharset       PASS 9/9     PASS 9/9
CharsetEncoderReach      PASS 14/14   PASS 14/14
CharsetEncoderReach2     PASS 11/11   PASS 11/11   <- includes new ConsoleHandler()
CharsetEncoderReach3     PASS 7/7     PASS 7/7
```

## 5. What is NOT claimed

* CratonVM answers **UTF-8** where HotSpot answers the console encoding
  (`Cp1251` on this host, from `stdout.encoding`). That difference predates this
  change, is what the existing override already committed to, and is not
  addressed here — the fix makes the object CONCRETE, not the encoding
  faithful. A separate question worth its own measurement.
* `charset_alloc`'s other callers are untouched. `native_charset_for_name` still
  fabricates, and on a real image it does not win — `Charset.forName` runs the
  JDK's own bytecode, which is why every `forName(...).newEncoder()` in the
  probe already passed.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out PrintStreamCharset
```
