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


---

## 9. Progress

### Wave 1 — 2026-09-11, 140 rows over 10 classes of `java/io/` and `java/nio/`

`RETIRED_SHADOW_L4_TRIPLES` in `native-api/src/retired_shadow.rs`, whose doc
comment carries the method and the arithmetic. §9.1 is the ledger §8 asks for;
§9.2, §9.3 and §9.4 are the three findings that change how the next wave should
be run, and they cost this wave five of its seven builds.

The classes: `java/io/File` (51), `java/io/DataInputStream` (18),
`java/io/DataOutputStream` (15), `java/io/ByteArrayOutputStream` (13),
`java/io/FilterOutputStream` (5), `java/nio/ByteBuffer` (34), and the four
`java/nio/ByteBufferAsCharBuffer{B,L,RB,RL}.order()` views.

### 9.1 The ledger — every bucket-A/B row in the prefix set, with a disposition

Over the **union of thirteen probe dumps**, not one run: §1's table is a
single-dump census and undercounts a class whose rows only one workload reaches.

| disposition | rows |
|---|---:|
| `RETIRED` — wave 1 | **140** |
| blocked: no probe in this tree invokes it | 479 |
| blocked: `sun/nio/ch/` package verdict, 2026-08-19 | 308 |
| blocked: outside a dial-floor family | 131 |
| blocked: lane T owns `concrete_receiver.rs:185` | 49 |
| blocked: `java/nio/file/Files`, armed 4, above the floor | 39 |
| deferred: `java/io/PrintStream`, its own wave (§5) | 30 |
| **backed out by a build** (five families, §9.2) | **51** |
| blocked: `Path` carrier is stamped with the interface | 11 |
| reviewed `Intrinsic` | 10 |
| **total** | **1 254** |

**"No probe invokes it" is the largest bucket and it is not a verdict about
those rows.** `invocations` is a lower bound — schema 5 carries an
`invocations_complete` bit to say so — and 479 rows being unmeasured by this
instrument is a statement about the instrument. Over half are one shape:
`jdk/internal/foreign/layout/ValueLayouts$Of*Impl`, eight classes at ~16 rows
each, which a single FFM layout probe would move into the funnel in one pass.
That is the cheapest next wave in the lane.

### 9.2 The dial is not a model of a retirement

The dial declines a native at DISPATCH, and the decline is **conditional**: when
the receiver's own class has no concrete body to yield to it answers no and the
native runs anyway — `declined_no_bytecode`, 4 156 of 180 268 on this wave's
scope. A retirement has no such fallback. **Every row the dial scored 0 on
*because it declined to decline* is unmeasured by it.**

So a per-family sweep at DIFF 0, and the same families re-armed together as one
scope at 0 diffs over 4 416 rows with 176 112 real yields, still lost two
families on the first build: `java/nio/file/spi/FileSystemProvider` (the VM's
provider is fabricated onto the ABSTRACT class, so `createLink` has no body, so
the dial ran the native — retired, the call reaches the abstract declaration and
throws) and `java/nio/CharBuffer` (`subSequence(1,3)` answered `cd` for `bc`;
the real bodies take their window from `position()`/`limit()`, which this VM's
carrier does not keep where the real accessors read them).

`java/nio/ByteBuffer`'s 34 rows — same accessors, same probe — moved nothing.
CharBuffer is a carrier defect, not a buffer-wide one.

**Use the dial to BOUND a wave. Never to certify one.**

### 9.3 Screen against the CORPUS, not only the probe tree

The 13 probes scored 0 in **both modes** on the built binary. The `--jdk-only`
corpus went **132/132 → 129/132**, reproducibly, on that same binary:

```text
  RFileTimes       every timestamp reads 1970-01-01T00:00:00Z
  RJdkSecurity     a property-named truststore IGNORED: 122 anchors for 1,
                   and a certificate HotSpot REJECTS is ACCEPTED
  RSslLiveSession  fails at client.responseCode = 200
```

`RJdkSecurity` is the one to read twice. It is not a crash and not a diff in a
probe — it is a **silently widened trust set**, which is precisely the failure
mode §4 says this lane owns, and no happy-path probe would ever have seen it.

`RFileTimes` has one cause, and it is the SETTER: `Files.setLastModifiedTime`
reads `FileTime.toMillis()` off a fabricated carrier, gets 0, and stamps the
file at the epoch — so all four read-back rows follow from one write. Dial-armed
attribution names `java/nio/file/attribute/` for it and
`java/io/ByteArrayInputStream` for `RSslLiveSession`.

**And the screen that WOULD have caught all three is build-free:** arm the dial
on the whole wave scope and run `regression-suite/run.sh` with
`CRATONVM_ARGS=--jdk-only`, against its own unarmed run on the same binary.
~20 minutes, versus a ~26-minute build plus a ~40-minute verification. On the
trimmed scope it reads:

```text
  unarmed control                  132 passed, 0 failed
  armed on the wave-1 scope        132 passed, 0 failed
```

Phase 2 already had this as its fourth precondition. §7 step 6 lists the corpus
*after* the build; it belongs in step 2 as well.

### 9.4 `RJdkSecurity` was a carrier defect, and it is fixed

`RJdkSecurity` reproduced under **no** dial scope, single or combined — §9.2's
blind spot exactly. What found it was not the dial and not the 13 probes but an
eight-line probe, `apps/probes/L4AbsPath.java`, printing path SHAPES:

```text
  temp.getPath          abs=true len=32 slashes=2      (correct)
  temp.isAbsolute       false                          (HotSpot: true)
  temp.getAbsolutePath  abs=true len=45 slashes=5      (the cwd, prepended)
```

Every `java.io.File` this VM builds kept its path in **slot 0** and wrote
nothing else. Self-consistent for exactly as long as this VM's own natives are
the only readers — §1.4's whole story — and over the moment real `File` bytecode
runs: `UnixFileSystem.resolve` reads `prefixLength`, never written and therefore
`0`, and calls every path relative. `javax.net.ssl.trustStore` then named a path
that no longer resolved, the JDK fell back to `cacerts` without throwing, and
the vector's one expected trust anchor became 122.

Fixed in the commit before this wave: `native-api/src/file_layout.rs`, the
shared carrier rule, applied at all **six** producing call sites across
`native-io` and `native-builtins`. Additive — slot 0 keeps the String, so the
twenty-two `read_file_path` readers are untouched. With it, the corpus is back
at 132/132 and the six `prefixLength`-dependent `File` rows (`isAbsolute`,
`getAbsolutePath`, `getAbsoluteFile`, `getCanonicalPath`, `getCanonicalFile`,
`toURI`) are retired in this table rather than carved out of it.

**Two lessons, and the second is the expensive one.** A fabricated carrier is a
STAMP and CONTENTS, and this lane keeps finding the contents empty — `Path`,
`CharBuffer`, `FileTime`, `FileSystemProvider` and now `File`, five for five.
And the first fix for this one measured *inert*: it was written into
`native-io`'s three `File` constructors, which are themselves retired by this
very table and so never run. The `File` under test came from `createTempFile`'s
producer in `native-builtins`. **A carrier rule belongs in `native-api`, beside
`path_layout` and `appended_slots`, and every producer has to be found first.**

**The instrument that would have attributed it in one run now EXISTS**, built
2026-09-11 off the back of this wave: `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns
named rows of the retirement tables back off at runtime, so the bisection that
cost this wave three builds is a sequence of runs.

```text
  CRATONVM_UNRETIRE_NATIVE_SHADOW=all                          is it the wave?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/                     which package?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File                 which class?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File.isAbsolute()Z   which row?
```

It resolves each rule against the tables **at arm time and prints the row
count**, because the failure this instrument would otherwise invite is the one
that makes bisection worse than useless: a mistyped rule matches nothing, the
vector still fails, and the reader concludes the triple is exonerated. A rule
reaching zero rows is named on stderr before any dispatch.

See [`crate::unretire`] (`native-api/src/unretire.rs`). The eighteen triples
this section leaves un-attributed for `RJdkSecurity` are now bisectable in
eighteen runs by anyone who wants the answer the carrier fix made moot.

### 9.5 What the residual work before it was

Wave 1 rests on three defects fixed on 2026-09-10 (`6f507bb48..44f28fe64`),
because the dial could not read a clean floor until they were:

* `FileInputStream.skip` was not an `lseek`, and `isRegularFile0` — the
  predicate deciding whether `skip0` runs at all — answered **false for every
  file**, having read an instance native's `args[1]` as its first argument. That
  is why two earlier, correct `skip0` bodies measured inert and were reverted;
* a synthetic `java.nio.file.Path` wrote its string to slot 0 and its filesystem
  to slot 1 on a two-slot object, where `UnixPath` is
  `fs(0) path(1) stringValue(2) hash(3) offsets(4)`. Now resolved by name
  against the implementation class, once per process, in
  `native-api/src/path_layout.rs`;
* a `ByteBuffer` view's read-only flag was read from the wrong carrier.

### 9.6 What is left

* **`java/io/PrintStream` (30).** Measured clean in the wave-1 dial screen and
  deliberately not in the table; §5 requires its own wave and its own commit.
  Read §9.2 and §9.3 before trusting that screen — and screen it against the
  corpus, where `System.out` actually lives.
* **479 rows no probe in this tree reaches.** `ValueLayouts$Of*Impl` is the
  cheapest half.
* **The five backed-out families (51).** All five are blocked on the same thing:
  a fabricated carrier whose real fields this VM never writes. §9.4 is what
  fixing one looks like, and it is the unlock — not retrying the retirement.
* **`java/nio/file/Path` (11) and, behind it, `FileSystemProvider`.** One defect
  and a measured order: the Path carrier is stamped with the INTERFACE, so
  `toString()` lands on `Object.toString()` and no `instanceof UnixPath` in the
  JDK's own `java.nio.file` code can succeed. Minting a concrete provider while
  that is true dies in `UnixPath.toUnixPath`'s `instanceof` (`L4FilesSweep`
  0 → 172). **Path first**, and it needs `concrete_receiver::alloc_concrete`
  *and* `mirror_class_registrations` together, plus every
  `class_name == "java/nio/file/Path"` test in `vm/` and `native-collections`.
* **`sun/nio/ch/` (308).** Package verdict upheld, not beaten.
* **`jdk/internal/foreign`.** Untouched; its prefix is not admitted, because
  nothing under it reached the candidate set.
