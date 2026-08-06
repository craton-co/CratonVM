# A `--features synthetic-jdk` binary run in real-JDK mode fails six suite classes the shipping build passes

| | |
|---|---|
| **Status** | OPEN — six classes named and reproduced, one instance of the family already fixed |
| **Severity** | high **for measurement** — this is the configuration the vm test gate and the regression suite are usually run with |
| **Modes** | built `--features synthetic-jdk`, run `--real-jdk`. The default `cratonvm-cli` build is unaffected |
| **Opened** | 2026-08-06, while fixing the blocking-queue instance of the same family |

## The measurement

Same source tree, same JDK 25 image, same host, same `regression-suite/run.sh`
— only the binary's Cargo features differ:

| binary | result |
|---|---|
| `cargo build --release -p cratonvm-cli` (the shipping default) | **29 passed, 0 failed** |
| `cargo build --release -p cratonvm-cli --features synthetic-jdk` | 23 passed, **6 failed** |

The six: `RStrings`, `RSerial`, `RCrypto`, `RChannelInterrupt`, `RFileTimes`,
`RNioNoFollow`. Every one passes in the default build.

A seventh, `RExecutorShutdown`, was in this set until 2026-08-06 and is the
worked example — see the retired
`threadpoolexecutor-drops-queued-tasks-and-never-terminates` write-up.

## The mechanism, from the one instance already solved

The `synthetic-jdk` Cargo feature decides which natives are **compiled and
registered**. The `--real-jdk` / `--synthetic-jdk` launcher flag decides which
**class library** is loaded. They are independent, so a feature-enabled binary
run against the real JDK registers synthetic natives on top of real JDK classes.

Where a synthetic surface models a different field layout than the real class,
the object becomes half-native: methods that have natives use the side layout,
and the first method without one runs real bytecode against state the synthetic
`<init>` never initialised. The blocking-queue case answered `offer -> true`,
`size() -> 0`, and then NPE'd inside real `poll(timeout)` on a null `takeLock`.

`native-api/src/registry.rs` already has the countermeasure — the
`drop_real_layout_synthetic` flag, set in every real-JDK arm, with a per-class
list of synthetic surfaces to drop so the real bytecode runs. `StringReader`,
`EnumSet`, `Pattern`/`Matcher`, the `Piped*` streams, `Permissions`,
`ScheduledThreadPoolExecutor`, the `Executors` pool factories and (as of
2026-08-06) the whole blocking-queue family are on it. The six classes below are
simply subsystems nobody has walked yet.

## How to close each one

The blocking-queue fix is the template, and it is cheap:

1. Run the failing class against both binaries to confirm it is this family and
   not a real defect: `CV=<default-build> bash regression-suite/run.sh` with
   `ONLY=<class>` must pass while the feature build fails.
2. Write the smallest probe that shows the divergence — usually a
   two-line "mutate, then read" against the suspect JDK type. The blocking-queue
   one was `offer(x); size()`.
3. Find the synthetic registrar for that type and confirm the real JDK bytecode
   is self-contained (no missing native it depends on). This is the step that
   decides whether the answer is "drop the surface" or "complete it".
4. Add the class to the `drop_real_layout_synthetic` family test.
5. Re-run the class in BOTH builds and in `--synthetic-jdk` mode — the synthetic
   mode output must be byte-identical, since the flag is only set in real-JDK
   arms.

First guesses from the names: `RStrings` is likely a `String`/`StringBuilder`
surface (there is already a `wp8_10_9_string_contains_native` witness test in
this area), `RFileTimes` and `RNioNoFollow` are `java.nio.file` attribute
surfaces, `RChannelInterrupt` is the interruptible-channel machinery, `RCrypto`
and `RSerial` are the crypto and serialization stubs. Each is its own
adjudication — "drop it" is right only where the real bytecode stands alone.

## Why this matters beyond the six

**An A/B measurement taken with a feature-enabled binary attributes these
failures to `dev`.** That happened three times in the sessions that produced
this page: each reported "7 pre-existing dev failures, identical on both arms"
and treated the set as the project's baseline. The A/B conclusions themselves
held — both arms shared the instrument — but the baseline was the instrument's,
not dev's, and `dev` was in fact green.

Until the six are closed: state which binary a suite number came from, and
prefer the default build for any claim about `dev`'s health.
