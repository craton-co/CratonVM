# Lane 4 — `java.io`, `java.nio`, `sun.nio`, and the FFM internals

**Scope: 1,110 §1.4 shadows over 131 classes, from 615 registration sites.**
Prefixes: `java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign`.
The largest prefix lane, and the one with the highest blast radius.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. Shape of the lane

```text
  54  java/io/File                       21  java/nio/DirectByteBuffer
  42  sun/nio/ch/SocketChannelImpl       18  java/io/DataInputStream
  39  java/nio/file/Files                18  jdk/internal/foreign/layout/...
  38  java/nio/ByteBuffer                17  java/nio/CharBuffer
  34  sun/nio/ch/DatagramChannelImpl     17  jdk/internal/foreign/MemorySessionImpl
  29  java/io/PrintStream               239  jdk/internal/foreign*  (total)
  26  sun/nio/ch/ServerSocketChannelImpl
```

## 2. Two package verdicts already on record — respect them or beat them per-triple

`RETIRED_SHADOW_PREFIXES` carries notes you must read before proposing anything
under `sun/nio/`:

- **`sun/nio/fs/` is admitted** (2026-08-20). Deliberately the narrow prefix.
- **`sun/nio/ch/` scored 34/36 on the 2026-08-19 dial sweep and "nothing under
  it is retirable"** as a package. Phase 2 then narrowed that to exactly **one**
  triple, measured on its own, and the note in the source says explicitly that
  this narrows rather than overrules the package verdict.
- **`java/lang/ref/` is absent on purpose.**

The rule those notes encode: *a prefix admits a package to the binary search;
the table decides what is retired.* A package verdict is the right default.
Beat it one triple at a time with that triple's own measurement, or not at all.

## 3. Channels overlap lane T — coordinate before you start

> **WITHDRAWN 2026-09-10 — the channel rows are YOURS, and there is no hold.**
> `concrete_receiver.rs:185` is class-parameterised, which is what hides it from
> the source-scanning drift gate, but it is not CROSS-LANE: all 191 of its goal
> rows are under `sun/nio/`, so lane 0 §2's rule puts every one of them in this
> lane, and lane 0's 1,100 for LT excludes them (1,100 + 191 is not 1,100).
> Measured from `--dump-native-registry --explain-jdk-only`; see [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md) §0.
> Take them with the rest of your `sun/nio/ch` surface — and take §2's package
> verdict as the default while you do.

`native-io/src/concrete_receiver.rs:185` is a **cross-lane registrar**: 191 rows
over 21 classes, including `SocketChannelImpl`, `DatagramChannelImpl` and
`ServerSocketChannelImpl`, which are your three biggest `sun/nio/ch` classes.
Lane T owns it whole.

So roughly 100 of your channel rows are **not yours to retire individually**.
Check lane T's page for a hold on that call site, and take the channel classes'
remaining hand-written rows only.

## 4. The failure mode this lane owns: a silent wrong answer

Every other lane's worst case is a null or an exception. Yours is corrupted
data, and it will pass a happy-path probe:

- **`ByteBuffer`/`CharBuffer`/`DirectByteBuffer` (76 rows)** — position, limit,
  mark and capacity invariants. A retirement that changes when `position` is
  advanced produces correct-looking output with the wrong bytes. Probe
  `flip`/`rewind`/`compact`/`slice`/`duplicate` **state triples** after each
  operation, relative reads mixed with absolute reads, and the
  `BufferUnderflowException`/`IllegalArgumentException` boundaries — with
  messages, since the route decides the text.
- **`jdk/internal/foreign` (239 rows)** — `MemorySessionImpl` liveness and
  layout arithmetic. A wrong `byteOffset` is a memory-safety bug, not a diff.
  Probe closed-session access, confinement violations from another thread, and
  alignment failures, and require each to *throw the right type*.
- **A fix that turns a loud failure into a rare silent one is not a fix.** This
  lane has already produced one: every metric moved the right way *and* the
  store was corrupted. Prefer a probe that reads data back and compares content
  over one that checks a return code.

`native-builtins/src/panama.rs:2752` (21 rows, 3 classes) is cross-lane with
L2 — coordinate.

## 5. `PrintStream` (29 rows) — do this last, and say so in the commit

`System.out` and `System.err` are how **every lane** reads its probes. A
regression here reads as all nine lanes' probes failing simultaneously, and the
first instinct will be to blame the harness. Two rules:

- Retire `PrintStream` in its own wave, after the rest of `java/io`, and name
  the blast radius in the commit message.
- Its probe must write to a `ByteArrayOutputStream`-backed stream as well as to
  `System.out`, so a broken `System.out` cannot hide the evidence of itself.

Watch the encoding path: `sun.nio.cs.UTF_8.<clinit>` captures the
`JavaLangAccess` getter on the way past, and that interaction has already cost
one build. If your change moves when charsets initialise, re-read L2 §3.

## 6. `java/io/File` (54) and `java/nio/file/Files` (39) — the platform split

The largest single class and one of the most portable-looking. It is not
portable: separators, absolute-path rules, `getCanonicalPath` on a
non-existent path, and permission methods all differ by OS, and **the two shell
gates in this campaign are keyed `25/linux` and refuse on Windows**. Two
consequences:

- Your probe rows must not embed a platform's path syntax as an expected value.
  Compare cratonvm against HotSpot **on the same host**, which is what the A/B
  scripts do.
- A `sun/nio/fs/` retirement measured only on one OS is measured on one OS. Say
  which, in the commit.

`File`'s methods that answer about a missing file (`exists`, `length`,
`lastModified`, `canRead`) return sentinel values rather than throwing —
another place a wrong answer is silent.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run.
2. Probe + HotSpot oracle, on this host. No build needed.
3. Fill `RETIRED_SHADOW_L4_TRIPLES`, sorted and unique.
4. Build token (L0 §5); one build per wave — this lane's waves should be large,
   since its probes are expensive to write and cheap to re-run.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count. **Never a timing claim** — `RMapGcStress` read 146 s then 334 s on the
   same binary.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the `sun/nio/ch` package verdict either upheld or beaten per-triple with the
measurement recorded, and every buffer/FFM retirement backed by a probe that
reads data back rather than checking a return code.
