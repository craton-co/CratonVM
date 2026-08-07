# FIXED — a `--features synthetic-jdk` binary run in real-JDK mode failed suite classes the shipping build passes

| | |
|---|---|
| **Status** | **FIXED 2026-08-07** — all 7 closed. The feature build now measures exactly what the shipping build measures |
| **Severity** | high **for measurement** — this is the configuration the vm test gate and the regression suite are usually run with |
| **Modes** | built `--features synthetic-jdk`, run `--real-jdk`. The default `cratonvm-cli` build is unaffected |
| **Opened** | 2026-08-06 |
| **Closed** | 2026-08-07 |

## The split

Same source tree, same JDK 25 image, same host, same `regression-suite/run.sh`
— only the binary's Cargo features differ:

| binary | result |
|---|---|
| `cargo build --release -p cratonvm-cli` (the shipping default) | **31 passed, 0 failed** |
| `cargo build --release -p cratonvm-cli --features synthetic-jdk` | 26 passed, **5 failed** |

Two more surfaced later (`RChannelInterrupt`'s second half, `RFileTimes`),
making seven in all. All seven are now closed:

| class | root cause | closed |
|---|---|---|
| `RExecutorShutdown` | the blocking-queue family (retired `threadpoolexecutor-drops-queued-tasks-and-never-terminates`) | 2026-08-06 |
| `RSerial` | `java/io/StringWriter` squatting `Writer.lock` | 2026-08-06 |
| `RStrings` + `RCrypto` | the charset family fabricating ABSTRACT-class instances | 2026-08-07 |
| `RChannelInterrupt` + `RNioNoFollow` | `native_fc_open` fabricating an abstract `FileChannel` | 2026-08-07 |
| `RFileTimes` | two defects: the cfg-guarded `FileOutputStream` `<init>` block, and a jar bridge missing from this arm | 2026-08-07 |

Final state, one dev tip (`51d68e1b7`), four binaries, ABBA-interleaved:

| build | change | suite |
|---|---|---|
| feature | none | 25 passed, 6 failed |
| feature | **all three fixes** | **28 passed, 3 failed** |
| default | none | 28 passed, 3 failed |
| default | all three fixes | 28 passed, 3 failed |

The feature build and the shipping build now agree class for class. The three
remaining failures — `RBlockingQueue`, `RSocketChannelInterrupt`,
`RMapGcStress` — fail in BOTH builds and are therefore `dev`'s own, not this
family. `--synthetic-jdk` MODE output is unchanged (11 passed / 20 failed on
both arms, identical verdicts).

**Not part of this family:** `RSocketChannelInterrupt` started failing on `dev`
on 2026-08-07 and fails in the DEFAULT build too, so it is a plain `dev`
regression rather than a feature-vs-default divergence. Do not fold it into this
page. The same now goes for `RBlockingQueue` and `RMapGcStress`.

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

## The seven, with measured divergence

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

**FIXED 2026-08-07 — and the earlier "a drop-list entry is not the fix here"
note on this page was WRONG.** It was recorded after dropping only
`CharsetEncoder`/`CharsetDecoder`, seeing `getBytes` throw, and concluding the
surface was load-bearing. The throw was not evidence of that; it was the next
layer of the same defect showing through, and the note was written without ever
reading the exception text. Whoever hits a partial result like that: capture the
exception before drawing the conclusion.

The family has to be dropped TOGETHER, and the order it was added in is the
evidence:

| dropped | `"abc".getBytes()` | `getBytes("UTF-8")` | `getBytes(UTF_8)` |
|---|---|---|---|
| nothing | `00 00 00` | `00 00 00` | `00 00 00` |
| + `CharsetEncoder`/`Decoder` | `AbstractMethodError: CharsetEncoder.encodeLoop has no Code attribute` | same | same |
| + `Charset` | `61 62 63` | `61 62 63` | `AbstractMethodError: Charset.newEncoder has no Code attribute` |
| + `StandardCharsets` | `61 62 63` | `61 62 63` | `61 62 63` |

Each step exposes the next fabricated ABSTRACT instance: the coder comes from a
`Charset`, and the standard charset object comes from its own registrations. All
four names are now in the `drop_real_layout_synthetic` family, so the real
`sun.nio.cs` classes get constructed and every overload matches HotSpot.

Measured on one dev tip, four builds:

| build | change | suite |
|---|---|---|
| feature | none | 25 passed, 6 failed |
| feature | charset family dropped | **27 passed, 4 failed** — `RStrings` and `RCrypto` green, nothing new |
| default | none | 30 passed, 1 failed |
| default | charset family dropped | 30 passed, 1 failed — **identical**, the shipping build is untouched |

A/B/B/A on the feature build, and `--synthetic-jdk` mode output is
byte-identical between the arms.

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
is `RandomAccessFile.getChannel`, which this path does not go through.

**FIXED 2026-08-07 — the third producer does not call
`alloc_concurrent_synthetic` at all**, which is exactly why two rounds of
grepping for that helper missed it. It is `native_fc_open` in
`native-io/src/lib.rs`, registered on

```
java/nio/channels/FileChannel.open(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)…
```

— the precise overload the suite calls — and it fabricates with a bare
`alloc_object(FileChannel, 2)`. It intercepted `FileChannel.open` *before the
provider was ever consulted*, which is the whole reason instrumenting the
`newFileChannel` closure printed nothing. The negative result was real; it was
pointing one frame too low.

Two defects in one registration, only the first of which the suite named:

* the receiver is the abstract class, so `write(ByteBuffer, long)` — which has
  no native, unlike the no-position `write(ByteBuffer)` — resolves to an
  abstract declaration and throws `AbstractMethodError`. The same object also
  has no `interruptor`, the field `AbstractInterruptibleChannel.begin()`
  dereferences, so it could never have produced the
  `ClosedByInterruptException` this vector exists to assert;
* the body is documented "simplified" and calls `open_read`, **ignoring the
  `OpenOption[]` entirely**. `FileChannel.open(p, WRITE)` returned a READ-ONLY
  fd, and `FileChannel.open(link, …, NOFOLLOW_LINKS)` followed the link — the
  defect `RNioNoFollow` gates, reached through a path that fix never touched.

The fix is to drop it in real-JDK mode (`drop_real_layout_synthetic`, scoped to
`open` by name). Nothing else has to be built: real `FileChannel.open` bytecode
calls `FileSystemProvider.newFileChannel`, which is already force-listed in
`native_override.rs` and already routed to the base-class registration, and that
shim's RECONCILE-WITH-REAL block already constructs a genuine
`sun.nio.ch.FileChannelImpl`. Its synthetic fallback is left in place, so a host
where the real construction fails keeps today's behaviour rather than a new one.

The instance natives on the class (`read`/`write`/`position`/`size`/`close`)
stay registered: they still serve that fallback object, and they do not
intercept a real `FileChannelImpl`, whose own declarations win because native
dispatch keys on the resolved method's declaring class.

`RFileTimes` and `RNioNoFollow` did **not** reproduce from the naive one-liner
(a plain `JarOutputStream` round-trip and a plain symlink `writeString` both
behave correctly), so their triggers are narrower than the table suggests —
start from the test source, not from the summary line.

### `RFileTimes`: two defects, and neither is about file times

**1. The `FileOutputStream`/`FileInputStream` `<init>` block was guarded by the
wrong thing** — and that guard silently un-did a fix that was already in the
tree.

`native-io/src/lib.rs` carries a comment block (FOS-FIX, 2026-05-20) explaining
that native `<init>` overrides must NOT be registered for the real-JDK build:
the real constructor allocates the `fd` `FileDescriptor` and calls `open0`, and
overriding it puts the fd in instance slot 0 — the *reference*-typed `fd` field,
where a `Value::Int` write is silently dropped — leaving every `write`/`flush`/
`close` a no-op. The comment is exactly right. The guard under it was
`#[cfg(feature = "synthetic-jdk")]`, which asks what was COMPILED when the
question is which CLASS LIBRARY was LOADED. So the feature build reproduced the
2026-05-20 defect in full:

| | feature build | default build | HotSpot 25 |
|---|---|---|---|
| `new FileOutputStream(f)`, `write(5 bytes)`, `close()` | **0-byte file** | 5 | 5 |
| `new FileOutputStream(path)` (String ctor) | **0** | 5 | 5 |
| single-byte `write(int)` × 5 | **0** | 5 | 5 |
| `fos.getFD()` | **throws** | ok | ok |
| `Files.write` / `Files.newOutputStream` / `RandomAccessFile` | 5 | 5 | 5 |

The last row is why this hid for so long: the three spellings most code uses
were fine. `RFileTimes` reported it as `readAttributes.size` = 0 and then as an
unreadable archive (`Could not find EOCD`) — the `JarOutputStream` it built on a
`FileOutputStream` had written nothing.

Fixed by making the guard the runtime one: a new
`NativeMethodRegistry::drops_real_layout_synthetic()` getter, read at the
registration site. The `cfg` stays as well — in a default build these natives
should not even be compiled in. This is the general lesson of this whole page in
one line: **`#[cfg(feature)]` is never the right guard for "is a real JDK on the
other end".**

**2. The jar/zip bridge was missing from this arm.** `vm_init.rs` has two
real-JDK arms — one inside the feature build, one in the shipping build — and
they had drifted. `register_p59_jar` (plus `register_p59_bulk_stream_transfer`
and `register_p59_zip_output_primitives`) was registered only in the shipping
one, so the feature build fell back to `native-io`'s `zip_real_jar` surface,
which builds every entry with `alloc_zip_entry`:

| | `JarFile.entries()` element | `getJarEntry` | `getEntry` |
|---|---|---|---|
| feature build | `java.util.zip.ZipEntry` | `ZipEntry` | `ZipEntry` |
| default build | `java.util.jar.JarEntry` | `JarEntry` | `JarEntry` |
| HotSpot 25 | `java.util.jar.JarFile$JarFileEntry` | same | same |

`JarFile.entries()` is declared `Enumeration<JarEntry>`, so the implicit
checkcast at the call site threw `ClassCastException: java.util.zip.ZipEntry
cannot be cast to java.util.jar.JarEntry` — for any caller that iterates a jar,
not just this test. Fixed by registering the three in the feature build's
real-JDK arm, in the shipping arm's order (the registry is last-write-wins).

**The residual worth knowing about.** Those three are not the only difference
between the two real-JDK arms. Diffing the `register_*` calls in each:

* in the shipping arm and NOT in the feature arm: `register_classvalue_natives`,
  `register_essential_natives`, `register_p67_misc`, `register_phase57_file`,
  `register_random_and_securerandom_natives`, `register_random_natives`,
  `register_spring_boot_logback_apply` (and the three now fixed);
* in the feature arm and not the shipping one: ~30, mostly the JMX/management
  and MethodHandle clusters.

Some of that asymmetry is deliberate. Some of it is the next `RFileTimes`. The
structural fix is for the two arms to share one function; that was out of scope
here and is not something to attempt without a suite run per step.

## Why the remaining five were NOT a repeat of the last two

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
  **This prediction was wrong, and worth keeping as a caution:** it was not a
  dispatch bug. Dispatch was correct — it resolved on the receiver's real class,
  which genuinely WAS the abstract one because a native had fabricated it. "No
  Code attribute" says the resolved method is abstract; it does not say the
  resolution was wrong. Check what the receiver's class actually is before
  theorising about how it was reached.

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
6. **Verify the A and B binaries actually differ.** `sha256sum` them. A build
   script that stages arms by copying files can leave both arms holding the same
   source after an early failure, and an A/B of a binary against itself reports
   a clean, stable, completely meaningless "no change" — which reads as "the fix
   does nothing" and will send you looking for a second root cause that is not
   there. That happened on this page's `FileChannel` fix and cost a full
   four-binary matrix.

## Why this matters beyond the five

**An A/B measurement taken with a feature-enabled binary attributes these
failures to `dev`.** That happened in three consecutive sessions, each reporting
"7 pre-existing dev failures, identical on both arms" and treating the set as the
project's baseline. The A/B conclusions held — both arms shared the instrument —
but the baseline was the instrument's, not dev's, and `dev` was green.

Now that they are closed the two builds agree, but the habit still earns its
keep: state which binary a suite number came from, and use the default build for
any claim about `dev`'s health. The arms can drift again — see the residual
`vm_init.rs` arm diff under `RFileTimes` — and the next drift will be just as
invisible as this one was.
