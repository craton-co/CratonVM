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
| `RStrings` | `"héllo".getBytes(UTF_8)` | `68 c3 a9 6c 6c 6f` | `00 00 00 00 00 00` — right LENGTH, zeroed content, so every decode round-trip fails |
| `RCrypto` | `MessageDigest.getInstance("SHA-256").digest("abc")` | `ba7816bf…15ad` | `709e80c8…147c` |
| `RChannelInterrupt` | `FileChannel.write(ByteBuffer, long)` | `3` | `AbstractMethodError: …FileChannel.write(Ljava/nio/ByteBuffer;J)I has no Code attribute` |
| `RFileTimes` | writes a jar, reopens it | round-trips | `JarFile … is not a valid zip: Could not find EOCD` |
| `RNioNoFollow` | `Files.writeString(symlink, …, NOFOLLOW_LINKS)` | refuses, target untouched | test asserts the target WAS touched |

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
