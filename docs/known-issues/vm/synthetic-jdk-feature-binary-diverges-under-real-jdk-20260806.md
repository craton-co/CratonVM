# A `--features synthetic-jdk` binary run in real-JDK mode fails five suite classes the shipping build passes

| | |
|---|---|
| **Status** | OPEN — 2 of 7 closed; the remaining 5 are diagnosed below, each with its measured divergence |
| **Severity** | high **for measurement** — this is the configuration the vm test gate and the regression suite are usually run with |
| **Modes** | built `--features synthetic-jdk`, run `--real-jdk`. The default `cratonvm-cli` build is unaffected |
| **Opened** | 2026-08-06 |

## The split

Same source tree, same JDK 25 image, same host, same `regression-suite/run.sh`
— only the binary's Cargo features differ:

| binary | result |
|---|---|
| `cargo build --release -p cratonvm-cli` (the shipping default) | **31 passed, 0 failed** |
| `cargo build --release -p cratonvm-cli --features synthetic-jdk` | 26 passed, **5 failed** |

Remaining: `RStrings`, `RCrypto`, `RChannelInterrupt`, `RFileTimes`,
`RNioNoFollow`. Every one passes in the default build.

Closed so far: `RExecutorShutdown` (the blocking-queue family — see the retired
`threadpoolexecutor-drops-queued-tasks-and-never-terminates` write-up) and
`RSerial` (`java/io/StringWriter`, 2026-08-06).

## The mechanism

The `synthetic-jdk` Cargo feature decides which natives are **compiled and
registered**. The `--real-jdk` / `--synthetic-jdk` launcher flag decides which
**class library** is loaded. They are independent, so a feature-enabled binary
run against the real JDK registers synthetic natives on top of real JDK classes.

Where a synthetic surface models a different field layout than the real class,
the object goes half-native: methods that have natives use the side layout, and
the first method without one runs real bytecode against state the synthetic
`<init>` never initialised.

`native-api/src/registry.rs` has the countermeasure — the
`drop_real_layout_synthetic` flag, set in every real-JDK arm, with a per-class
list of synthetic surfaces to drop so the real bytecode runs. `StringReader`,
`EnumSet`, `Pattern`/`Matcher`, the `Piped*` streams, `Permissions`,
`ScheduledThreadPoolExecutor`, the `Executors` pool factories, the blocking-queue
family and `StringWriter` are on it.

**The recurring authoring error this list exists to catch:** a registration is
put behind `#[cfg(feature = "synthetic-jdk")]` with a comment saying "let the
real bytecode run by default", and that is believed to be the whole fix. It is
not — the Cargo feature only decides what is compiled. The runtime half is the
drop-list entry. `StringWriter` carried exactly that comment and exactly that
gap.

## The remaining five, with measured divergence

Each line is what a two-line probe shows in the feature build under
`--real-jdk`, next to HotSpot 25.

| class | probe | HotSpot | feature build |
|---|---|---|---|
| `RStrings` + `RCrypto` | `"abc".getBytes(…)` | `61 62 63` | `00 00 00` — right LENGTH, zeroed content. **One bug, two classes** — see below |
| `RChannelInterrupt` | `FileChannel.write(ByteBuffer, long)` | `3` | `AbstractMethodError: …FileChannel.write(Ljava/nio/ByteBuffer;J)I has no Code attribute` |
| `RFileTimes` | writes a jar, reopens it | round-trips | `JarFile … is not a valid zip: Could not find EOCD` |
| `RNioNoFollow` | `Files.writeString(symlink, …, NOFOLLOW_LINKS)` | refuses, target untouched | test asserts the target WAS touched |

### `RStrings` and `RCrypto` are the same defect, and it is not crypto

`RCrypto` fails a SHA-256 known-answer test, which reads as a crypto defect. It
is not. The digest is **correct**; the input is wrong:

```
observed on the feature build : 709e80c88487a2411e1ee4dfb9f22a861492d20c4765150c0c794abd70f8147c
sha256(00 00 00) on HotSpot   : 709e80c88487a2411e1ee4dfb9f22a861492d20c4765150c0c794abd70f8147c
```

`RCrypto` digests `"abc".getBytes("UTF-8")`, and in this configuration every
`String.getBytes` overload — no-arg, `(String)`, `(Charset)`, for UTF-8, ASCII
and ISO-8859-1 alike — returns a correctly-sized, **zero-filled** array. So
`RCrypto` and `RStrings` collapse into one bug. Fix `getBytes` and both classes
should go green.

What is *not* the cause, each measured rather than assumed:

* **Not the store primitive.** `write_prim_element`'s `Byte` arm accepts
  `Value::Int` (`gc/src/heap.rs`), and a bytecode `byte[]` store works
  (`b[0]=97` reads back 97).
* **Not a bogus array.** The result is a genuine `[B` of the right length whose
  zeros are visible through indexing, `java.lang.reflect.Array.get` and
  `System.arraycopy` alike.
* **Not any of the four registered `getBytes` natives.** Marking all four
  (`charset.rs`'s no-arg and `(Charset)` forms, `phases_early.rs`'s `(String)`
  and `(Charset)` forms) with an `eprintln` and rebuilding produced **no output**
  — none of them runs. `String.getBytes` is not force-listed, so in real-JDK
  mode the real bytecode wins and the corruption happens underneath it.

The stack is `String.getBytes` → `String.encode` → `String.encodeWithEncoder`
→ `CharsetEncoder.encode`. `register_p58_charset_coder`
(`native-builtins/src/phases_late/charset_buffers.rs`) models a coder as a
3-slot object and reads `charset()` / `averageBytesPerChar()` /
`maxBytesPerChar()` straight out of field indices 0/1/2. A real
`sun.nio.cs.UTF_8$Encoder` has an entirely different layout, so those accessors
answer with unrelated fields. `charset.rs` already carries a comment predicting
exactly this — "a real-JDK HeapCharBuffer/HeapByteBuffer whose field layout does
not match our synthetic 5-field Buffer overlay used by the encoder native — so
the encode loop reads zero chars".

**A drop-list entry is NOT the fix here, measured.** Adding
`CharsetEncoder`/`CharsetDecoder` to `drop_real_layout_synthetic` does not
restore correct bytes: it makes `getBytes` *throw* inside
`CharsetEncoder.encode` instead of returning zeros, and the suite stays at five
failures. Unlike `StringWriter` and the queue family, this surface is
load-bearing in real-JDK mode — the real encoder path depends on it. The fix has
to make the coder natives correct for a real receiver (resolve the fields by
name, or detect a real encoder and defer), not delete them.

### `RChannelInterrupt`: the receiver is the ABSTRACT class

`AbstractMethodError: java/nio/channels/FileChannel.write(Ljava/nio/ByteBuffer;J)I
has no Code attribute` is not a missing native. It is a receiver whose runtime
class IS the abstract class, so every method that has no native to intercept it
resolves to an abstract declaration:

| | `FileChannel.open(...).getClass()` |
|---|---|
| HotSpot 25 | `sun.nio.ch.FileChannelImpl` |
| default build | `sun.nio.ch.FileChannelImpl` |
| feature build | **`java.nio.channels.FileChannel`** (superclass `AbstractInterruptibleChannel`) |

Even the one-arg `write(ByteBuffer)` fails on it, not just the positional
overload the suite happens to report.

Two things ruled out by measurement:

* **Not class resolution.** `Class.forName("sun.nio.ch.FileChannelImpl")`
  answers identically in both builds — the real class, 65 declared methods,
  with the 7-arg `open` present. So the concrete class IS available to the
  feature build.
* **Not the `newFileChannel` fallback.** `register_phase57_nio_file`'s
  `FileSystemProvider.newFileChannel` shim already tries to build a real
  `FileChannelImpl` first (the RECONCILE-WITH-REAL block) and only falls back
  to `alloc_concurrent_synthetic("java/nio/channels/FileChannel", 1)` if that
  fails. Instrumenting that closure with `eprintln!` and rebuilding produced
  **no output at all** — it never runs for `FileChannel.open`. The abstract
  instance comes from a producer that is still unidentified.

The other `alloc_concurrent_synthetic("java/nio/channels/FileChannel", 1)` site
is `RandomAccessFile.getChannel`, which this path does not go through. **Next
step: find the third producer** — instrument `alloc_concurrent_synthetic` itself
for that class name, or breakpoint on the allocation, rather than auditing
registration sites by eye (two rounds of that found the wrong two).

`RFileTimes` and `RNioNoFollow` did **not** reproduce from the naive one-liner
(a plain `JarOutputStream` round-trip and a plain symlink `writeString` both
behave correctly), so their triggers are narrower than the table suggests —
start from the test source, not from the summary line.

## Why the remaining five are NOT a repeat of the last two

The two closed cases were easy because the offending class had **exactly one
production registration site, and it was feature-gated** — so a class-keyed drop
reproduces the default build by construction. Verify that property before
reaching for the same fix:

* `java/io/StringWriter` — one site, `#[cfg(feature = "synthetic-jdk")]`. The
  other hits are `#[cfg(test)]`. Safe to drop by class name.
* `java/security/MessageDigest` — `register_security_natives` is **ungated**,
  and `native-builtins/src/jca/message_digest.rs` holds the real implementation.
  A class-keyed drop would remove the surface the default build KEEPS. This one
  needs the category-aware escape hatch (`self.effective_category()`, as
  `keep_real_scheduled_executor_bridge` already does) or a fix at the registrar.
* `String.getBytes` — `charset::register_real_charset_natives` is called in
  **both** builds (`vm_init.rs:1930` and `:2620`), and there are competing
  `getBytes` registrations in `phases_early.rs`, `lib.rs` and `charset.rs`.
  Registration is last-write-wins, so the divergence is an **ordering**
  difference between the two arms, not a missing gate. Find which registration
  wins in each build before changing anything.
* `java/nio/channels/FileChannel` — the "no Code attribute" shape says dispatch
  resolved to the ABSTRACT method rather than a concrete implementation, which
  is a different failure from a layout squat. Treat it as a dispatch bug first.

## How to close one

1. Confirm it is this family: `ONLY=<class> CV=<default-build> bash regression-suite/run.sh`
   must pass while the feature build fails.
2. Write the smallest probe that shows the divergence — "mutate, then read", or
   just call the one method.
3. Enumerate **every** production registration for that class and check which
   are feature-gated. This is the step that decides whether a class-keyed drop
   is correct or would break the default build.
4. Confirm the real JDK bytecode is self-contained (no missing native it needs).
5. Add the drop rule, then re-run the class in BOTH builds and in
   `--synthetic-jdk` mode — the synthetic-mode output must be byte-identical,
   since the flag is only ever set in real-JDK arms.

## Why this matters beyond the five

**An A/B measurement taken with a feature-enabled binary attributes these
failures to `dev`.** That happened in three consecutive sessions, each reporting
"7 pre-existing dev failures, identical on both arms" and treating the set as the
project's baseline. The A/B conclusions held — both arms shared the instrument —
but the baseline was the instrument's, not dev's, and `dev` was green.

Until the five are closed: state which binary a suite number came from, and use
the default build for any claim about `dev`'s health.
