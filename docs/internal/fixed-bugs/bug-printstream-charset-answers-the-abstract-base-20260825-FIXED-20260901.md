# `PrintStream.charset()` answers the ABSTRACT `java.nio.charset.Charset`, so anything that encodes through it dies

**Status: CLOSED 2026-09-01.** The `AbstractMethodError` was fixed 2026-08-25.
The two residuals §5 left open — the encoding this VM reports for the platform
and for the standard streams, and `charset_alloc`'s remaining callers — are
measured and closed in **§6**, which is the last section of this record.
Present in **both** modes, not just `--jdk-only`.

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

## 5. What is NOT claimed *(as written 2026-08-25 — both rows are closed in §6)*

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

## 6. 2026-09-01 — the two residuals §5 named, measured and closed

§5 left two things open. Both were measured on `2d866cb28` (`origin/dev`) before
anything changed here, and both are closed below.

### 6.1 The encoding was UTF-8 because it was PINNED, in five places that JEP 400 did not pin

The comment above the property table said *"Encodings — JDK 18+ pinned to UTF-8
for stdout/stderr/file/native"*. JEP 400 pinned **`file.encoding`**, and only
`file.encoding`. `native.encoding` is the platform's own text encoding, and the
three stream encodings follow the console on Windows and the locale on Unix.
Five keys were pinned; one of them was allowed to be.

That is not a cosmetic difference in a property string. `System.out`'s charset
is derived from `stdout.encoding`, and `printstream_encode` /
`install_real_stream_fields` both encode through the charset object stamped on
the stream — so the pin decided the BYTES on fd 1.

MEASURED, one CratonVM binary (`2d866cb28`) against Temurin 25.0.3+9, the same
Linux host, `probes/EncodingFidelity.java` and
`regression-suite/src/REncodingFidelity.java`:

```text
                        LANG=C.UTF-8                    LANG=C
                   HotSpot      CratonVM         HotSpot           CratonVM
file.encoding      UTF-8        UTF-8            UTF-8             UTF-8
native.encoding    UTF-8        UTF-8            ANSI_X3.4-1968    UTF-8      <-
sun.jnu.encoding   UTF-8        UTF-8            ANSI_X3.4-1968    UTF-8      <-
stdout.encoding    UTF-8        UTF-8            ANSI_X3.4-1968    UTF-8      <-
stderr.encoding    UTF-8        UTF-8            ANSI_X3.4-1968    UTF-8      <-
stdin.encoding     UTF-8        UTF-8            ANSI_X3.4-1968    UTF-8      <-
System.out.charset UTF-8        UTF-8            US-ASCII          UTF-8      <-
defaultCharset     UTF-8        UTF-8            UTF-8             UTF-8
System.out.print("[Ж]")
                   5b d0 96 5d  5b d0 96 5d      5b 3f 5d          5b d0 96 5d
                                                 i.e. "[?]"        i.e. "[Ж]"
```

The last row is the one that matters and is why this is a defect rather than a
naming difference: under a C/POSIX locale — a cron job, a container, a systemd
unit, the default for most non-interactive processes — HotSpot puts three bytes
on fd 1 and CratonVM put four, with the high bit set, through a stream that
both VMs agreed was called `stdout`. Anything that trusts the declared encoding
(a fixed-width record writer, a protocol framer, a terminal) is handed bytes it
cannot represent.

The `--jdk-only` arm answered identically to the compatible arm in all four
combinations, so this was never a mode-specific defect — the same thing §1 had
to retract about the original report.

**The fix.** `cratonvm_native_api::os_encoding` is the one place that asks the
host:

* **Unix** — `setlocale(LC_CTYPE, "")` then `nl_langinfo(CODESET)`. The
  `setlocale` is not optional and is the whole subtlety: a process starts in
  the `C` locale regardless of the environment, so `nl_langinfo` answers
  `ANSI_X3.4-1968` for EVERY locale until it is called. `LC_CTYPE` rather than
  HotSpot's `LC_ALL`, and the previous locale is restored, so nothing else in
  the process (C code's `printf("%f")` in particular) changes underfoot.
* **Windows** — `GetACP()` for `native.encoding`/`sun.jnu.encoding`, and the
  attached console's code page for a std stream that has one. The two use
  DIFFERENT spellings and that is measured, not guessed: on a 1251 host, in one
  `-XshowSettings:properties` run with stdout redirected and stdin still on the
  console, HotSpot answers `stdout.encoding = Cp1251` (the ACP) and
  `stdin.encoding = cp866` (the console). `Cp<n>` for the ACP, `cp<n>` /
  `ms<n>` (in the 874..=950 band) for a console.

Both bootstrap property tables now ask that helper — `SharedVm::new`'s
`sys_props` in `vm/src/vm/vm_init.rs` and the `props` table in
`native-builtins/src/system_bootstrap.rs`. That file's own comment warns that
the two overlap without agreeing and that **a key added to one is added to
neither**; this is the same rule read the other way.

`install_charset` stamps `Charset.forName(<the stream's own encoding>)` instead
of a hard-coded UTF-8, and `PrintStream.charset()`'s repair path asks
`printstream_repair_encoding` which of the three answers applies — because
`System.out.charset()` and `new PrintStream(baos).charset()` are US-ASCII and
UTF-8 respectively in the SAME HotSpot run, and one constant cannot be right
for both.

Three smaller things fell out and are part of the fix:

* `ANSI_X3.4-1968` had to enter the shared alias table
  (`cratonvm_native_api::charset::canonical_charset_name`) along with the rest
  of `sun.nio.cs.US_ASCII`'s alias list. It is what glibc answers in the C
  locale, so it arrives at `Charset.forName` at bootstrap the moment the VM
  stops pinning UTF-8, and an unmapped name there is an
  `UnsupportedCharsetException` out of `initPhase1`.
* `printstream_encode` read the abstract stand-in as "UTF-8 unconditionally".
  That was true only while the stamp was a hard-coded UTF-8. It now encodes
  through the VM's own engine by the stand-in's `name` slot, because a stream
  that CLAIMS `US-ASCII` and emits UTF-8 is the one outcome worse than either
  honest answer.
* `sun.stdout.encoding` / `sun.stderr.encoding` are no longer seeded. They are
  the JDK 8..18 spelling; JDK 19 replaced them with the unprefixed keys, and
  both read **null** on Temurin 25.0.3+9. Seeding them was drift in the same
  family, pointing the other way.

**What is still NOT claimed.** The Unix path reports `nl_langinfo(CODESET)`
verbatim. HotSpot has a small table that rewrites a few platform spellings on
AIX/Solaris; the two rows this host can produce (`UTF-8`, `ANSI_X3.4-1968`) are
passed through by HotSpot too, and only `C`, `C.utf8`, `POSIX` and `en_US.utf8`
are generated here, so a row that cannot be measured is not guessed at. The
Windows console/ACP split is measured on one host at one code page; the band
rule around it comes from the JDK's `getConsoleEncoding` and is covered by unit
tests rather than by a second machine.

And a real degradation, stated because it is a behaviour change and not a
theoretical one: if the host names an encoding this image has no charset for
(a synthetic-JDK build on a `cp866` console), `install_charset` degrades the
WHOLE stream to UTF-8 rather than stamping a name it cannot encode with. The
property still reports the host's answer; the stream does not pretend to.

### 6.2 `charset_alloc`'s other callers — measured unreachable, changed anyway

§5 said these were untouched. `probes/CharsetConcrete.java` asks all twenty
doors that can hand back a `Charset` and then CONSUMES each one
(`newEncoder`/`newDecoder`/`contains`), which is where the `AbstractMethodError`
actually lands:

```text
                              HotSpot   CratonVM compatible   CratonVM --jdk-only
CharsetConcrete               20/20     20/20                 20/20
Charset.availableCharsets()   173       173                   173
```

`173`, not `11`, is the load-bearing number: it is the real JDK's own charset
provider answering, which means `native_charset_available_charsets` — and, with
it, `Charset.forName`, `Charset.defaultCharset` and the six
`StandardCharsets.*` accessors — never wins in either real-JDK mode. They are
registered, but the dispatch gate only promotes a native over an ABSTRACT or
absent method, and all of these are concrete in a real image. So the residual
was not reachable from anywhere this VM is measured.

They were converted to `charset_concrete_or_synthetic` regardless. The helper
asks the real `Charset.forName` first and falls back to `charset_alloc`
unchanged, so on the paths above it is a no-op, and in a synthetic-JDK image it
is exactly today's behaviour — while the trap that produced this whole record
(an instance of the ABSTRACT base, whose `newEncoder()` has no `Code`
attribute) stops being one edit away from any future caller.

`native_charset_for_name` is deliberately NOT converted: it IS the
`Charset.forName` shim, and routing it through a helper whose first move is to
call `Charset.forName` would recurse.

### 6.3 The ratchet

`regression-suite/src/REncodingFidelity.java`, scheduled in `CORE_CLASSES`,
18 checks. It asserts no particular encoding — it publishes the eight
properties, the four charset names, the three encoder outcomes and the three
byte readings on `CK` lines, and the suite DIFFS those against HotSpot in the
same environment. That is what makes it locale-independent: it catches a re-pin
under `LANG=C` and equally a wrong console code page on Windows, without the
vector having to know which host it is on.

The last check is deliberately not hex. `CK REncodingFidelity direct=[Ж]`
writes the character through the real `System.out`, which is the only one of
the three byte readings that exercises the encoder this VM's own `print`
natives use rather than one the vector constructed — and that encoder is the
half of this a property table alone could not have fixed.

### The re-run after merging dev

Everything above was measured on the branch before it took `origin/dev`. dev
moved 52 commits under it, so it was rebuilt and re-run on the merge:

```text
                        before the merge      after the merge
regression suite        81/81 · 121/121       82/82 · 122/122   (dev added a vector)
harness-blindness       0 · 0                 0 · 0
REncodingFidelity       PASS · PASS           PASS · PASS
RBufferPoolCount        PASS · PASS           PASS · PASS
RJdkJmx                 PASS · PASS           PASS · PASS
```

The post-merge binary is built with `lto = "thin"` and `codegen-units = 16`
on six crates instead of the release profile's `fat` / `1`. That is not a
preference: the shared host SIGKILLed the fat-LTO link of `cratonvm-cli` five
times and `cratonvm-native-builtins` twice more, with `MemAvailable` at 0-1 GiB
and load between 90 and 270 on 8 cores from other sessions' builds. Same
sources, same `opt-level`; it is a correctness oracle and not a performance
one, and every number in this record that is about SPEED — there are none —
would need the standard profile. The pre-merge rows above are from a standard
`--release` build.
